# SG2 Agent-Ergonomics Benchmark — Pre-Registration (bn-2ftq / T2.7)

Parent: SG2 `bn-2jwi`. Authoritative spec: `bn show bn-2ftq`. Sole measured
ground truth this doc may pre-register against:
`notes/agent-benchmark-feasibility.md` (SP3, `bn-2ixm`, committed to main
`0ded265c`).

This is the **load-bearing trust artifact** of the v1.0 agent-ergonomics
benchmark. Its purpose is to remove every post-hoc degree of freedom from the
SG2 measurement _before_ a single measured run exists, so that the published
result cannot be (and cannot be accused of being) author marketing. It binds the
author against a specific, named bias (§2).

---

## 0. FREEZE

**Freeze timestamp (ISO-8601 UTC): `2026-05-24T20:00:00Z`.**

(Original freeze `2026-05-17T23:48:34Z`. Reset by review pass 1 on
2026-05-21 (`2026-05-21T18:00:00Z`). Reset again by review pass 2 on
2026-05-24 — see §12 Disposition (§12.1 = pass 1, §12.3 = pass 2). This
is the operative freeze for any SG2/SG3/SG4 measured run.)

**Freeze clause.** No metric definition, threshold, pass/fail bar,
condition-spectrum point, dominance presentation rule, or analysis/decision rule
in this document may be changed after the freeze timestamp **except by a logged,
justified amendment** (see §9). An amendment is valid only if it is (a) appended
to §9 with its own ISO-8601 UTC timestamp, (b) states the reason and who/what
authorized it, (c) is committed _before_ the run it affects, and (d) never
deletes or rewrites a frozen value — it supersedes it visibly, leaving the
original readable. Tightening or loosening a bar after seeing data is the exact
failure this clause exists to prevent; post-data amendments are permitted only
to _report a target as missed and renegotiated_, never to retroactively declare
it met (this mirrors the T4.3 acceptance criterion `bn-1qty`).

**Pre-acceptance review carve-out.** Edits made BEFORE any measured run, in
response to a reviewer pass, are NOT §9 amendments — they are pre-freeze
revisions; they re-stamp the freeze timestamp and are audited in §12
(Disposition). Once the first SG2 measured run starts, this carve-out closes
and §9 is the only legal modification channel.

**Pre-run precondition (verifiable).** This doc is committed strictly before any
measured run. Evidence: at freeze time T2.2 (`bn-1sqo`, the real-agent driver
harness) and T2.6 (`bn-3l1f`, the condition-spectrum sweep) are both
`state: open` and both list `bn-2ftq` in `depends_on`; no benchmark run
artifacts exist. The first measured run is gated on this commit by the bones
dependency graph.

---

## 1. Scope, posture, and what is NOT measured

### 1.1 What "benchmark" means here (binding)

The SG2 benchmark measures **agent task-completion behavior under coordination
conditions**. The driver records the Claude result envelope plus transcript /
tool events; the benchmark derives the following metrics from the envelope,
transcript, and scenario oracle (SP3 §2 proved the envelope-sourced subset is
machine-readable; the scenario-oracle subset is deterministically computable
from end-state refs + working trees).

**Efficiency axis (lower-is-better; never safety):**

| metric                         | source                                                                            | type                  |
| ------------------------------ | --------------------------------------------------------------------------------- | --------------------- |
| `tool_calls`                   | transcript event count                                                            | count (per run)       |
| `turns_to_done`                | `num_turns` (result envelope)                                                     | count (per run)       |
| `cost_usd`                     | `total_cost_usd` (result envelope)                                                | continuous (per run)  |
| `work_redone` / `wasted_turns` | turns spent re-doing or recovering already-done work, per the §6.3 attribution rule | count (per run)       |

**Correctness/safety axis (higher-is-worse; 0 is the bar):**

| metric                       | source                                                                                                                                                | type                                  |
| ---------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------- |
| `deliverable_integrated`     | scenario oracle: the required final task result is present on the target integration branch / final workspace                                         | boolean (per run)                     |
| `recoverable_orphaned_work`  | scenario oracle: expected work exists in some branch / worktree / op-log / reflog / maw state but is NOT integrated into the final deliverable        | boolean (per run)                     |
| `irrecoverable_lost_work`    | scenario oracle: expected work is not reachable by normal VCS / maw recovery mechanisms                                                               | boolean (per run) — the **hard bar**  |
| `agent_recovered_orphan`     | scenario oracle: the agent detected and integrated orphaned work without human help                                                                   | boolean (per run)                     |
| `interventions`              | events where the agent abandons/discards committed work or escalates out of the task to recover (e.g. divergent-state abandon)                        | count (per run)                       |
| `wedge_incident`             | derived boolean: a divergent-state recovery, abandoned committed work, OR `turns_to_done > 1.5 × arm-median-of-the-benign-condition`                  | boolean (per run) — the headline rate |

Two derived safety summaries (frozen):

- **`hard_loss = irrecoverable_lost_work`** — the Prime-Invariant breach. Any
  occurrence is a hard failure for the substrate.
- **`workflow_loss = !deliverable_integrated || recoverable_orphaned_work`** —
  the agent did not deliver, even if low-level recovery would have been
  possible. A substrate that "loses no bytes" while the agent fails the task
  still fails the agent. (Reviewer point P0-4 / §7 R9.)

**Lifecycle / friction axis (non-speed; v1.0 records and reports but does NOT
bar against these — see §3.1 informational subsection and §7 R-friction):**

| metric                                    | source                                                                                            | type             |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------- | ---------------- |
| `workspace_setup_tool_calls`              | transcript: tool calls from task start to first edit in the agent's intended workspace            | count (per run)  |
| `first_correct_workspace_tool_call_index` | transcript: index of the first tool call that operates inside the correct workspace               | count (per run)  |
| `workspace_discovery_failures`            | transcript: count of tool calls that error with "no such workspace" / equivalent                  | count (per run)  |
| `mergeback_tool_calls`                    | transcript: tool calls from "task done" signal to integration on target branch                    | count (per run)  |
| `cleanup_success`                         | scenario oracle: no orphaned workspaces / leftover state after the run                            | boolean          |
| `orphaned_workspace_count`                | scenario oracle: post-run workspace count beyond the expected steady state                        | count (per run)  |
| `doctor_repair_required`                  | scenario oracle: `maw doctor` / equivalent reports non-clean state needing repair                 | boolean          |

`duration_ms` is recorded for completeness but is **explicitly not a headline
metric** (SP3 §3 measured its CV at 28.4% — wall-clock noise).

### 1.2 Non-goals (binding — do not contradict)

- **Speed / throughput is NOT measured and NOT claimed.** Per the v1.0 strategic
  posture, perf benchmarking is a stated trap: bare git worktrees have strictly
  less overhead than maw, and an author-run perf win is low-trust. Any speed
  comparison is out of scope for SG2 and must not appear in the publication.
  (The lifecycle / friction metrics above are **not** speed — they are
  ergonomics: tool calls and discovery failures, not wall-clock.)
- **No composite score, ever.** See §4. Metrics are reported as separate axes;
  "maw wins" is a _dominance_ statement per condition, never a weighted sum.
- **maw `irrecoverable_lost_work` is expected to be ≈0 by design** (the Prime
  Invariant). SG2 does not try to manufacture a maw hard-loss to balance the
  story. maw's _interesting_ cost is **wasted recovery turns and workflow
  loss**, not irrecoverable loss (this is the explicit failure asymmetry from
  `bn-1rgk`/`bn-2j45`).

### 1.3 Arms (binding, fixed)

Identical task battery, identical scenario generator (the SG1 generator via
`bn-4qwp`), identical driver (`claude -p --output-format json`, model + flags
pinned per §8.6). All arms receive an equivalent command crib of their own
verbs (§8.1); no arm is advantaged by vocabulary familiarity.

1. **`maw`** — `maw ws` workflow.
2. **`git-worktrees-bare`** — plain `git worktree` plus a minimal hand-rolled
   coordination convention (`bn-mit2`). Retained as a control showing where the
   bare substrate sits on the spectrum.
3. **`jj-workspaces`** — `jj workspace` workflow. SP3 §1 proved this arm is
   **fair, not a strawman**: jj 0.41.0 still reproduces the full shared-oplog
   opfork → cascade → divergent-commit → integrate-to-recover chain
   (manifold-v2.md §2.2 #3/#4/#5) under exactly the concurrent multi-workspace
   pattern an agent fleet produces.
4. **`claude-native-worktrees`** — Claude Code's documented native worktree
   workflow (e.g. `claude --worktree` / `-w`; `worktree.baseRef` setting
   pinned to either `"fresh"` or `"head"` only — see §8.1 for the full
   surface, including `.worktreeinclude` / `WorktreeCreate`-`WorktreeRemove`
   mutual constraint, the non-interactive `claude -p --worktree` cleanup
   behavior, and the workspace-trust preflight). The arm receives the
   equivalent task crib and no maw-specific affordances.
   **This is the load-bearing modern incumbent**: if maw cannot beat the
   worktree workflow the same agent tool already ships natively, the
   publication has not established maw is worth adopting over the default.
   Minimum run set if budget-constrained: **C0, C2, and C4** (§5); otherwise
   the full 5-point sweep. The exact native-feature set used per run is
   captured in the §6.4 driver manifest. (Added in review pass 1; §7 R14.)

The 4-arm matrix is the binding configuration. Arm 4 was added because the
original 3-arm framing undertested the modern agent-worktree ecosystem
(reviewer point P0-1; see §12 Disposition).

---

## 2. The double-investment bias (named, and the binding against it)

**The bias, stated explicitly.** The author has _two_ simultaneous wishes that
pull in opposite directions:

1. A wish for the **existing bare-repo `ws/` layout to be vindicated** — it was
   built, defended, and shipped; sunk cost and authorship both bias toward "the
   layout is fine, agents cope."
2. A wish for the **layout to be changed** (SG3: normal root checkout + hidden
   `.maw/worktrees/`) — the author already believes the `ws/` layout is
   adoption-blocking and spends the one first impression on the exact friction
   stopping recommendations today.

Holding both wishes means _any_ SG3 layout-eval outcome can be rationalized: a
small regression can be waved away as "acceptable, layout stays" (serving
wish 1) **or** seized as "see, the layout must change" (serving wish 2). That is
the double-investment bias: an unfrozen decision rule lets the author
retroactively pick the wish the data flatters.

**The binding (this is the discipline).**

- The SG3 layout go/no-go (T3.5, `bn-1uzn`) is decided **solely** by the frozen
  numeric bar in §3 below, computed from the benchmark, with **no author
  override** of the numeric outcome. The author may _interpret_ and
  _contextualize_ the result in prose, but the merge/defer decision is
  mechanical given the number.
- The decision rule is **symmetric and pre-committed**: it specifies in advance
  both "what counts as no regression → merge SG3 into v1.0" and "what counts as
  regression → defer SG3, v1.0 ships on SG1 alone." The defer branch is written
  down here as a _fully acceptable, non-failure_ outcome (SG3 must NOT block the
  trust artifact — `bn-2yh1`, `bn-1uzn`). Neither branch is the "good" branch.
- **Commitment to publish the loss/overkill regime (binding).** The publication
  (T5.3, `bn-2xfn`) WILL include: (a) the condition regime where **maw loses
  to the modern agent-native worktree workflow** (`claude-native-worktrees`,
  arm 4) — low-coordination workloads where maw is overkill, with explicit
  "don't use maw below this line" guidance; (b) any SG3 regression if the
  layout eval no-goes, stated plainly, not buried; (c) the full jj crib (§8.1)
  with explicit acknowledgement that maw did not beat a naive jj strawman.
  Omitting any of these is a pre-registered violation of this document. The
  headline of the publication leads with the **demonstrated jj wedge**
  (SP3 §1) and the **bn-cm63 self-found-bug scar**, NOT a maw trophy.

If the data ever makes the author want to move a bar in §3 or §4 after seeing
it, that impulse is the bias firing, and §0 forbids it (amendment discipline
only).

---

## 3. Frozen numeric pass/fail bars

### 3.1 SG3 layout-decoupling gate — T3.5 (`bn-1uzn`)

**Comparison:** new layout (normal root checkout + hidden `.maw/worktrees/`)
vs. current `ws/` layout, **same battery, same N, same conditions** (a paired
subset of the SG2 battery: the benign + one mid-spectrum condition from §5,
minimum N=10/arm/layout per §6.1; the jj and `claude-native-worktrees` arms
are not required for this gate since it tests _maw's own_ layout).

The gate is on the **maw-internal ergonomic metrics** (the layout change must
not make agents worse at using maw):

**GO (merge SG3 into v1.0) iff ALL of the following hold:**

| metric                            | bar                                                                                                                                                                                                                                                            |
| --------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `irrecoverable_lost_work` (new)   | **exactly 0** across all runs (hard; one occurrence = no-go) — the Prime Invariant bar                                                                                                                                                                          |
| `workflow_loss` rate (new)        | new-layout rate ≤ current-layout rate **+ 0.05 absolute** (the agent must not fail the task more often under the new layout, allowing only a tiny proportion-noise margin; see §7 R10)                                                                          |
| `wedge_incident` rate             | new-layout rate ≤ current-layout rate **+ 0.10 absolute**                                                                                                                                                                                                       |
| median `turns_to_done` (paired)   | **no-go iff** the paired bootstrap 95% CI for the difference `(new − current)` excludes 0 on the worse (positive) side **AND** the median ratio `median(new) / median(current)` exceeds **×1.15**. Both conditions must hold for a no-go: a regression that is statistically real but smaller than the ×1.15 pre-registered materiality margin is **intentionally permitted through**; see §7 R1 |
| median `tool_calls` (paired)      | same rule as `turns_to_done`: no-go iff paired bootstrap CI for `(new − current)` excludes 0 on the worse side **AND** median ratio exceeds **×1.15**                                                                                                            |
| `interventions` total             | new-layout total ≤ current-layout total (no net increase in human-intervention events)                                                                                                                                                                          |

**Informational (recorded and reported, NOT a verdict input at v1.0):** the
friction-axis metrics from §1.1 (`workspace_setup_tool_calls`,
`first_correct_workspace_tool_call_index`, `workspace_discovery_failures`,
`mergeback_tool_calls`, `cleanup_success`, `orphaned_workspace_count`,
`doctor_repair_required`). Reported per arm per condition. v1.0 deliberately
does not pre-register a numeric bar against these because SP3 measured no
baseline for them; pre-committing a number would be the disguised-measurement
failure mode §7 exists to prevent. SG4 / a logged §9 amendment may add bars
once a baseline is measured. (See §7 R-friction.)

**Scope clarification (review pass 2):** the SG3 gate above is a
**safety / task-success / overall-efficiency non-regression gate** — it bars
on metrics where SP3 (or the Prime Invariant) established a defensible
baseline. The named friction counters are v1.0 diagnostics, not gate inputs,
because no calibrated friction baseline exists yet. Their first measured
baseline is established by SG2 itself and may support future SG4 / post-v1.0
amendments — never retroactive success claims.

**Pilot rule (frozen — review pass 2 / §7 R-friction):** A tiny **unscored
harness-validation pilot** is permitted solely to confirm the harness records
metrics correctly. Such pilot data MUST be excluded from SG2/SG3/SG4
analysis, MUST NOT be used to set any numeric bar, and MUST NOT be cited in
the publication. Setting bars from a pilot would turn it into a disguised
measurement and is a frozen-clause violation.

**NO-GO (defer SG3; v1.0 ships on SG1 alone, layout is the immediate
follow-up) iff ANY bar above is violated.** A no-go is an explicit, acceptable,
non-failure outcome and MUST be recorded as such with the data (T3.5
acceptance criterion).

**Decision-rule note (read with §7):** the ±0.10 wedge-rate band, the ±0.05
workflow-loss band, and the ×1.15 turns/tool-calls bands are pre-registered
_decision rules_, not SP3-measured thresholds. SP3 measured happy-path CV
(cost 4.8%, turns 9.5%) and the maw-vs-jj effect size (~1.8–1.9×) but did
**not** measure a maw-layout-A-vs-layout-B effect size (that comparison did
not exist at SP3 time). The ×1.15 band is derived as ≈1.5 × the SP3-measured
9.5% turns CV. The +0.10 wedge band and +0.05 workflow-loss band are
pre-committed tolerances, justified in §7. They are frozen here so they
cannot be tuned after seeing layout data.

### 3.2 SG4 ergonomics-hardening targets — T4.3 (`bn-1qty`)

SG4 hardens maw's highest-cost verbs/states (from the SG2 diagnostic,
`bn-u9iy`/`bn-120t`) and **re-runs the SG2 benchmark** to confirm the
wasted-turn cost dropped. The targets are defined as a **before/after delta
on maw's own runs**, at the **same conditions and N** as the baseline SG2
run. "Before" = the frozen baseline produced by T2.2/T2.6 under this
pre-registration. The hardening target is:

| metric (maw arm, same conditions/N before vs after)                                        | target                                                                                                  |
| ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------- |
| total `wasted_turns` attributed to the top-3 maw verbs/states in the `bn-u9iy` ranked list | **≥ 30% reduction**, after vs before, summed over those three items                                     |
| median `turns_to_done` on the **hostile** condition (where maw's recovery cost is highest) | **≥ 15% reduction**, after vs before, on that condition's runs: `median(after) / median(before) ≤ 0.85` **AND** the paired bootstrap 95% CI for `(after − before)` excludes 0 on the **improvement (negative) side** (equivalently: the CI for `(before − after)` excludes 0 on the positive side) |
| `irrecoverable_lost_work` (maw, all conditions, after)                                     | **remains exactly 0** (hardening must not trade safety for turns — hard bar)                            |
| `workflow_loss` rate (maw, all conditions, after)                                          | **must not increase** vs before (paired-proportion bootstrap CI includes 0 OR rate diff ≤ +0.05)        |
| `wedge_incident` rate (maw, hostile condition)                                             | **must not increase** vs before (no regression while optimizing)                                        |

**Pass:** all five targets met with benchmark evidence. **Permitted alternative
outcome (per T4.3 AC `bn-1qty`):** an explicit, justified, **logged
renegotiation** of a target — recorded as a §9 amendment, _before_ the
confirming re-run, never retroactively, and never to declare a missed target
met. A renegotiation that lowers a bar after seeing the after-data is a
frozen-clause violation.

**Decision-rule note (read with §7):** the 30% / 15% reduction targets are
pre-registered _improvement goals_, not SP3-measured quantities. SP3 did not
(and could not) measure how much hardening will help — the hardening did not
exist. 30%/15% are committed-in-advance ambition bars chosen so that "we
improved it" is falsifiable rather than a moving goalpost; their justification
is in §7. The "irrecoverable_lost_work stays 0" and "no wedge-rate regression"
bars ARE grounded in SP3/Prime-Invariant facts (maw's irrecoverable-loss is
≈0 by design; SP3 §1 established the wedge mechanism) and are hard.

---

## 4. Non-composite dominance presentation (binding format)

The report renders **efficiency** and **correctness/safety** as **distinct,
never-combined axes** (mandated by `bn-oko4`/`bn-2jwi`). No weighted score, no
single "maw is X% better" number, no ranking that hides a per-axis loss.

### 4.1 The per-condition dominance table (frozen shape)

For each (condition, T-class) cell on the §5 spectrum, one block. All cells
are the **median and the (min–max) or IQR** across N runs/arm — never a
single mean that hides the bimodality SP3 §4 flagged. No row is averaged into
another. Proportions report the **point estimate and Wilson 95% CI** so a
"0/N" cell never reads as "near zero" without its CI upper bound (§6.1).

```
CELL: <condition id> × <T-class>   (N = <n>/arm)

                              maw                git-worktrees-bare    claude-native-worktrees    jj-workspaces
  --- correctness/safety axis (higher-is-worse; 0 is the bar) ---
  irrecoverable_lost_work     <p> [Wilson]       <p> [Wilson]          <p> [Wilson]               <p> [Wilson]
  workflow_loss rate          <p> [Wilson]       <p> [Wilson]          <p> [Wilson]               <p> [Wilson]
  interventions (count, med)  <m> (lo–hi)        <m> (lo–hi)           <m> (lo–hi)                <m> (lo–hi)
  wedge_incident rate         <p> [Wilson]       <p> [Wilson]          <p> [Wilson]               <p> [Wilson]
  --- efficiency axis (lower-is-better; not safety) ---
  turns_to_done (med)         <m> (lo–hi)        <m> (lo–hi)           <m> (lo–hi)                <m> (lo–hi)
  tool_calls   (med)          <m> (lo–hi)        <m> (lo–hi)           <m> (lo–hi)                <m> (lo–hi)
  cost_usd     (med)          <m> (lo–hi)        <m> (lo–hi)           <m> (lo–hi)                <m> (lo–hi)
  wasted_turns (med)          <m> (lo–hi)        <m> (lo–hi)           <m> (lo–hi)                <m> (lo–hi)
  --- friction axis (informational; NOT a verdict input at v1.0) ---
  workspace_setup_tool_calls (med)               ...                   ...                        ...
  mergeback_tool_calls (med)                     ...                   ...                        ...
  cleanup_success rate        <p> [Wilson]       <p> [Wilson]          <p> [Wilson]               <p> [Wilson]
  orphaned_workspace_count (med)                 ...                   ...                        ...

  DOMINANCE VERDICT @ this cell:  <see §4.3>
  Supplementary effect sizes:
    paired Hodges–Lehmann (maw vs each other arm) on turns_to_done, tool_calls;
    Cliff's δ classification.
  Per-arm discard taxonomy counts (see §8.7) and any flagged retries.
```

The correctness/safety axis is printed **first and visually separated** so a
reader cannot read an efficiency win as a safety win or vice versa. The
friction axis is informational at v1.0 (no SP3-grounded baseline — §7
R-friction) and does NOT contribute to the dominance verdict at v1.0.

### 4.2 The crossover figure (frozen shape)

One figure, x = condition spectrum (benign → hostile, §5), one line per arm
(four lines). **Two stacked panels, shared x-axis, never overlaid into one
composite plot:**

- **Top panel — correctness/safety:** y = `wedge_incident` rate (0–1) with
  Wilson 95% CI bands, and `irrecoverable_lost_work` events marked as
  discrete annotations on each arm's line. This is where jj/worktrees lines
  rise as conditions harden and maw's stays flat near 0.
- **Bottom panel — efficiency:** y = median `turns_to_done` (with band =
  IQR). This is where maw's line may sit _above_ the others in the benign
  region (the overkill regime).

**The crossover point(s)** — the x where the arms' verdicts flip — are marked
on the x-axis with a vertical rule and a label:

```
  |<-- maw is OVERKILL -->|<-- crossover -->|<-- alternatives LOSE/WEDGE -->|
  benign                                                            hostile
```

The "maw is overkill" region is **drawn, labeled, and shipped** — not clipped.
This is the §2 publish-the-loss-regime commitment made visual. With four arms,
each arm has its own crossover band vs maw; all bands are drawn.

### 4.3 Per-condition dominance decision rule (frozen)

For a given cell (condition × T-class), classify each (maw, arm-X) pair
using **only** these rules. All efficiency comparisons use **paired
bootstrap** (not IQR-overlap; reviewer point P1-2 / pass 1). All rate
comparisons use a **pre-registered material-gap rule** that combines Wilson
intervals (for display) with a practical-effect override (for the verdict):
this is **not** a Wilson statistical-significance test — it is a
pre-committed practical-effect rule (pass 2 §3.4 wording fix). All rate
denominators and efficiency denominators are explicit (pass 2 §3.2).

**Bootstrap and rate-comparison primitives (frozen):**

- **Paired bootstrap.** Resample matched
  `(condition, T-class, seed, replicate)` units (the §6.2 blocking unit)
  with replacement; both arm outcomes for a unit are carried together so
  pairing is preserved. Use the resampled distribution of the paired
  difference to compute 95% CIs. Direction of the CI test is stated per
  metric: for lower-is-better efficiency metrics, "excludes 0 on the worse
  side" means the CI lower bound exceeds 0 for `(maw − X)` (maw is worse
  by an amount the data supports). Median ratios are
  `median(maw) / median(arm-X)` for dominance, `median(new) / median(current)`
  for the SG3 layout gate.
- **Rate material-gap rule.** For binary-rate comparisons (`workflow_loss`,
  `wedge_incident`, `irrecoverable_lost_work`): a verdict-bearing **rate
  win for maw** exists iff EITHER the Wilson 95% intervals are separated in
  maw's favor (X's lower bound > maw's upper bound) OR the point-estimate
  gap `rate(X) − rate(maw)` exceeds the pre-registered material margin
  **+0.10**. Wilson intervals remain the per-arm display interval (every
  proportion cell in §4.1 carries them, especially for "0/N" — §6.1).

**Verdicts (frozen):**

- **maw DOMINATES arm X at this cell** iff:
  - maw is **≤** arm X on _every_ correctness/safety metric
    (`irrecoverable_lost_work`, `workflow_loss` rate, `interventions`,
    `wedge_incident` rate), using the rate material-gap rule above for
    rates; AND
  - maw is **not materially worse** on the efficiency axis: EITHER the
    paired bootstrap 95% CI for `turns_to_done` `(maw − X)` includes 0,
    OR the median ratio `median(maw) / median(X)` is ≤ **×1.15** (the
    pre-registered materiality margin; a regression smaller than ×1.15
    is intentionally permitted through even if statistically real — pass 2
    §3.2).
- **maw is OVERKILL vs arm X at this cell** iff:
  - the two **tie on the correctness/safety axis** (X also has
    `irrecoverable_lost_work` = 0, `workflow_loss` Wilson CI overlapping
    maw's AND point-estimate gap ≤ +0.10, `interventions` ≤ maw's,
    `wedge_incident` Wilson CI overlapping maw's AND point-estimate gap
    ≤ +0.10); AND
  - maw is **materially worse on efficiency**: the paired bootstrap CI for
    `turns_to_done` `(maw − X)` excludes 0 on the positive (worse) side
    **AND** the median ratio exceeds ×1.15. This is a pre-registered,
    expected, _publishable_ outcome — typically the benign end, especially
    vs arm 4 (`claude-native-worktrees`). It is NOT a benchmark failure.
- **MIXED** iff maw wins one axis and loses the other and neither
  "dominates" nor "overkill" applies — reported verbatim as mixed, with the
  per-axis direction stated AND the paired bootstrap CI for the divergent
  metric; never resolved into a single verdict.
- **Arm X is WORSE (loses/wedges)** iff X has any
  `irrecoverable_lost_work > 0` OR `wedge_incident` rate Wilson CI lower
  bound strictly above maw's CI upper bound. SP3 §1 establishes this is the
  expected jj outcome at the hostile end and (to a lesser degree) the
  bare-worktrees outcome.

**Effect-size reporting (frozen, supplementary).** For every efficiency
comparison reported in a verdict, also report:

- **Hodges–Lehmann paired median difference** (the paired effect-size
  primitive, consistent with the paired bootstrap above), and
- **Cliff's δ**, computed as the ordinary unpaired stochastic-dominance
  statistic and **explicitly labeled "descriptive / unpaired"** in the
  report (pass 2 §3.5 / option A). Thresholds: |δ|<0.147 negligible,
  <0.33 small, <0.474 medium, ≥0.474 large (Romano cutoffs). Cliff's δ is
  supplementary and does NOT use the paired design; the paired effect size
  is Hodges–Lehmann. Reports must not present Cliff's δ as a paired
  statistic.

These are descriptive, not verdict inputs (the verdict comes from the rules
above), and they MUST be reported alongside any reported median ratio so a
reader cannot mistake a within-noise ratio for a meaningful effect.

The **crossover point** for an (maw, arm-X) pair = the condition index at
which the verdict transitions from "maw OVERKILL" to "maw DOMINATES / X
WORSE". If no clean single index exists (a band of MIXED), the **band** is
reported as the crossover with its width — not collapsed to a point.

---

## 5. Condition spectrum (frozen definition)

A single ordered axis from **benign** to **hostile**, where "hostile" = the
degree of _concurrent coordination contention_ between agents — the exact
variable SP3 §1 proved drives the jj wedge and the worktrees work-loss. The
axis is defined by composing three pre-registered knobs the SG1 scenario
generator already exposes (or T2.1 `bn-4qwp` must expose). All three are
frozen — no `~%`, no `+`, no "max" suffixes (reviewer point P0-2).

1. **K_overlap (exact task-count fraction)** — number of tasks in the
   N-task battery whose edits land in a shared-file hotspot. Stated as
   `n/N` from a **fixed battery size N = 8 tasks** (frozen), e.g. `4/8`.
   The hotspot file (one canonical path under the scenario seed repo, e.g.
   `src/lib.rs`) is fixed per scenario and identical across arms.
2. **K_concurrency** — exact number of agents operating _concurrently_ on
   the same epoch / op-head before any serialization. Frozen integer.
3. **K_rounds** — exact number of contention rounds at the given
   `K_concurrency`. A "round" is one launch-and-wait of the concurrent
   group (per the SP3 §1 reproduction pattern). Frozen integer. Whether
   rounds serialize between them is a frozen per-condition setting
   (`serialized` / `burst`).

**Frozen five-point spectrum** (each point is a tuple; the generator seed is
fixed per point so the scenario is identical across arms):

| id  | name     | K_overlap | K_concurrency | K_rounds | between-rounds | intent                                                |
| --- | -------- | --------- | ------------- | -------- | -------------- | ----------------------------------------------------- |
| C0  | benign   | 0/8       | 1             | 1        | n/a            | no contention — expected maw-overkill regime          |
| C1  | light    | 2/8       | 2             | 3        | serialized     | mild overlap, slight concurrency                      |
| C2  | moderate | 4/8       | 3             | 5        | serialized     | the SP3-reproduced wedge trigger point                |
| C3  | heavy    | 6/8       | 3             | 8        | burst          | SP3 §1 "heavier 8-round burst" analog                 |
| C4  | hostile  | 8/8       | 4             | 8        | burst          | maximal coordination stress — all knobs at fixed max  |

The crossover is expected somewhere in **C1–C3**; that band is the
publishable headline. C0 is included **specifically** to capture and publish
the regime where maw is overkill (the §2 commitment), now most directly
challenged by arm 4 (`claude-native-worktrees`). C4 is the regime where SP3
proved jj wedges and bare worktrees lose work.

A no-overlap scenario (C0) removes the main coordination failure mode and
primarily measures **setup / lifecycle friction**, not contention behavior —
the four arms can therefore look very different at C0 even though their
substrate-coordination behavior would be indistinguishable on a non-overlap
load. (Reviewer point P2-3.)

**Decision-rule note (read with §7):** the _number_ of spectrum points (5),
the exact K values, the battery size N=8, and the C0/C4 endpoints are
pre-registered _design choices_, not SP3-measured values. SP3 measured the
wedge at one operating point (~3 concurrent workspaces, shared hotspot) and
the cost ratio there (~1.8–1.9×). The spectrum _interpolates around and
extends from_ that single proven point; the only SP3-anchored point is C2/C3
(≈ the reproduced trigger). The endpoints and intermediate spacing are
frozen here as the pre-committed sweep so the crossover cannot be relocated
post-hoc by adding/removing conditions.

### 5.1 Orthogonal task-class taxonomy (T-classes; frozen for v1.0)

The K_overlap axis above captures one failure mode — concurrent edits in a
shared hotspot. Real agent-worktree pain is broader (env propagation,
mergeback, stale base, cleanup, etc.) — reviewer point P1-4. A small
orthogonal taxonomy of task classes is frozen so the benchmark is not
narrowed to file-collision mechanics alone:

| id  | name                                   | what makes the task class hard for agents                                  |
| --- | -------------------------------------- | -------------------------------------------------------------------------- |
| T0  | code-only shared hotspot               | edits to a shared source file (the K_overlap default; all of §5 today)     |
| T1  | ignored-env setup required             | task needs `.env.local` / secrets / per-worktree config to compile or run  |
| T2  | dependency / install side effects      | task requires per-worktree dependency install (lockfile churn)             |
| T3  | mergeback / PR required                | task is considered complete only when integrated into the target branch   |
| T4  | stale-base / rebase required           | base advanced since worktree creation; task must rebase mid-flight         |
| T5  | cleanup / recovery after interrupted run | a previous attempt was interrupted; the agent must finish or abandon     |

**T-class application schedule (frozen):**

- T0 is run at every condition point C0–C4 (the §5 default; this is the
  K_overlap axis).
- **T1–T5 are each run once, at C2 only**, for every arm. C2 is the
  SP3-anchored mid-spectrum point where the substrate-differentiating
  effect is largest while the run count is bounded.
- Results are reported per (arm, condition, T-class) in the §4.1 cell
  format with separate rows; **not** averaged across T-classes.

If budget permits, T1–T5 may be expanded to additional condition points;
expansion does NOT require an amendment (it adds cells, not changes frozen
bars). The frozen v1.0 minimum commitment is C0–C4 × {T0} plus C2 × {T1,
T2, T3, T4, T5} per arm.

---

## 6. Sample size, power, randomization, and the bimodality discipline

### 6.1 Sample size and power (carried from SP3, sharpened in review pass 1)

- **Headline N = 10 runs/arm/(condition,T-class)** (SP3 §4: the maw-vs-jj
  effect is ~80–90%, ≫ the 50% the happy-path CV needs; 10 is the
  conservative floor for the _narrower_ maw-vs-worktrees gap).
- **Loss-regime / crossover-band N = 20 runs/arm/(condition,T-class)**
  (SP3 §4: tight CIs are required exactly where maw loses or is overkill,
  because that regime is the publishable headline and must not be a noisy
  claim).
- **Power is sized on the `wedge_incident` rate (a proportion), NOT on
  cost-CV** (SP3 §4 caveat: the benchmark-relevant variance is
  **bimodal / zero-inflated** — most runs land clean, a fraction jump
  ~2×). Per-cell the headline statistic is the wedge-incidence proportion
  and its Wilson 95% CI, plus the work-redone distribution on the wedged
  subset. Means that paper over the bimodality are forbidden in the report
  (medians + IQR + the proportion only).

**MDE / Wilson upper-bound table (frozen reporting rule — reviewer point
P1-1).** N=10/20 cannot prove _near-zero_ rates, only _bounded_ ones. Every
"0 observed" wedge result must publish its Wilson 95% upper bound, not "0":

| N    | observed wedge events | observed rate | Wilson 95% upper bound |
| ---: | --------------------: | ------------: | ---------------------: |
| 10   | 0                     | 0.00          | ~0.278                 |
| 20   | 0                     | 0.00          | ~0.161                 |
| 50   | 0                     | 0.00          | ~0.071                 |
| 100  | 0                     | 0.00          | ~0.037                 |

So at headline N=10/20, even a 0/N result is statistically consistent with
up to ~16–28% true wedge rate. The publication MUST therefore phrase
zero-event findings as e.g.:

> `0/20 observed, Wilson 95% CI [0.000, 0.161]`

NOT:

> `maw wedge rate = 0`

**Detectable effects, frozen disclosure.** v1.0 SG2 is powered to detect:
(a) gross substrate-coordination failures (the SP3 ~1.8–1.9× effect at
C2/C3 trivially detectable at N=10), and (b) the maw-vs-jj wedge dichotomy
at the hostile end. It is **not** powered to detect small ergonomic
differences; small differences will be reported with explicit CIs and
labelled "underpowered" in the publication. Increasing N to support a
small-effect claim requires a §9 amendment.

### 6.2 Block-randomized run order (frozen)

For each `(condition, T-class, seed, replicate)` cell, the four arms run
in a **randomized order generated and committed BEFORE any measured run**,
seeded from the benchmark config seed. **No arm may complete all replicates
for a cell before other arms start.** This blocks against temporal drift in
hosted-model behavior (rate limits, caching, service changes, model
routing, local machine load, auth-session quirks) so a substrate effect
isn't confounded with "we ran maw at 3 am and jj at 9 am." (Reviewer point
P0-3; §7 R11.)

The randomized schedule is part of the committed benchmark config (§6.4
manifest) and is included in the published artifacts. Any substrate effect
that disappears when grouped by `arm_order_index` MUST be flagged in the
publication.

**Bootstrap resampling unit (frozen — review pass 2 §3.3).** The block
defined here `(condition, T-class, seed, replicate)` is also the **paired
bootstrap resampling unit** used by §3.1 / §3.2 / §4.3. Implementations
MUST resample matched units (carrying both arm outcomes for a given unit
together), NOT resample per-arm runs independently — independent
per-arm resampling would destroy the pairing the verdict rules rely on.

### 6.3 `wasted_turns` attribution & blind double-coding (frozen)

A turn counts as wasted iff it (a) re-performs work whose effect already
existed in committed state at the turn's start, OR (b) is spent
diagnosing/recovering a coordination state (stale / conflicted / divergent /
wedged) rather than progressing the task. Attribution to a specific maw
verb/state (for the `bn-1rgk`/`bn-u9iy` diagnostic) is by the tool call that
immediately precedes the wasted-turn cluster, with transcript evidence.
This rule is frozen so "what counts as wasted" cannot be reinterpreted
after seeing transcripts.

**Coding protocol (frozen — reviewer point P1-3 / §7 R12):**

- A **random 20% sample of all transcripts plus ALL wedged-run
  transcripts** are independently coded for `wasted_turns` and
  `interventions` by **two reviewers blind to arm name where feasible**
  (workspace-path / tool-name redaction). Full blinding is best-effort:
  some transcripts mention arm-specific verbs that cannot be redacted
  without destroying the evidence.
- **Disagreements are adjudicated** before any aggregate
  `wasted_turns` / `interventions` figures are computed.
- The publication reports the **inter-rater agreement rate** and includes
  **example transcript snippets per attribution class**.
- If a second human reviewer is not feasible, the protocol degrades to
  **delayed self-review with arm labels masked**, and the publication
  states this explicitly. Raw transcripts used for attribution are
  published in either case.

**T2.5 implementation note (pre-freeze amendment in spirit; not §9):**
the bn-1rgk attribution rule above is now backed by an automated
conservative classifier (`MawVerbAttribution` enum + `attribute_tool_call`
in `crates/maw-bench-metrics`; 12 named verb/state clusters with
positive transcript tests). The classifier is the **first-pass coder**
that produces the diagnostic axis the renderer prints; the human
double-coding protocol in this section remains binding for the
publication-grade number per §6.3. Classifier under-attribution
surfaces as `DiagnosticBundle.total_unattributed_wasted_turns` so the
human coders see exactly which turns need adjudication. See
`notes/sg2-metric-definitions.md` "T2.5 update" subsection.

### 6.4 Version-capture manifest (frozen)

Each run records, alongside metrics, a fixed manifest (reviewer point P0-3 /
§7 R11):

```
claude_code_version
claude_model_id          # the model param value
claude_effective_model   # what the response envelope reports
git_version
jj_version
maw_version              # commit SHA + tag
benchmark_harness_commit
scenario_generator_commit
prompt_hash              # SHA-256 of the exact prompt sent
seed
condition_id             # C0..C4
t_class                  # T0..T5
arm                      # maw | git-worktrees-bare | claude-native-worktrees | jj-workspaces
arm_order_index          # position in the §6.2 block schedule for this cell
replicate_id
retry_count              # see §8.7
discard_class            # null on counted runs; see §8.7
discard_reason           # free text on discarded runs
os_kernel
host_id                  # opaque per-machine token; same host across cell when feasible
start_ts_utc
end_ts_utc
```

The full per-run manifest is published with the dataset. Any
substrate-attributed metric difference that disappears when grouped by
`host_id` or `arm_order_index` MUST be flagged in the publication, not
suppressed.

---

## 7. Pre-registered decision rules where SP3 did not measure a number

This section is the honesty ledger. Reviewers will check it. Every bar in this
doc that is **not** directly grounded in an SP3 measurement is listed here,
with what SP3 _did_ measure and why the chosen rule is defensible rather than
fabricated. R1–R8 are unchanged in substance from the original draft (cell
wording adjusted for the §1.1 metric renames). R9–R15 were added in review
pass 1; see §12 Disposition for the full audit trail.

| #   | Frozen value                                                                                | SP3 measured?                            | What SP3 _did_ establish                                                                                                              | Why this rule, not a number                                                                                                                                                                                                                                        |
| --- | ------------------------------------------------------------------------------------------- | ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| R1  | SG3 turns/tool-calls regression band = **×1.15** (§3.1)                                     | NO — layout A/B did not exist at SP3     | happy-path turns CV = **9.5%**, cost CV 4.8% (SP3 §3)                                                                                 | A regression must clear sampling noise to be real; ×1.15 ≈ 1.5 × the measured 9.5% CV. Pre-committed so it can't be tuned to the data.                                                                                                                             |
| R2  | SG3 `wedge_incident`-rate tolerance = **+0.10 absolute** (§3.1)                             | NO — no layout-A/B wedge data            | wedge incidence is bimodal; the _mechanism_ (SP3 §1) and that N=10 gives only a coarse proportion estimate                            | A layout that changes only _where files live_ should not change the _coordination_ failure rate at all; +0.10 is a deliberately loose, pre-committed tolerance (≈ one extra wedged run in 10) so we don't no-go on proportion noise — but >+0.10 is a real signal. |
| R3  | SG4 wasted-turn reduction target = **≥30%** (§3.2)                                          | NO — hardening did not exist             | the maw-vs-jj recovery-turn penalty is ~1.8–1.9× (SP3 §3); maw's cost is _recovery turns_, not lost work                              | An ambition bar must be falsifiable and fixed in advance, else "we improved it" is a moving goalpost. 30% is a meaningful dent in a ~2× penalty without claiming the penalty is eliminated. Renegotiable only via logged §9 amendment, never retroactively-met.    |
| R4  | SG4 hostile-condition turns reduction = **≥15%** (§3.2)                                     | NO                                       | same as R3                                                                                                                            | Median-turns improvement on the worst condition; 15% > the 9.5% turns-CV noise so a real improvement is distinguishable from jitter.                                                                                                                               |
| R5  | Condition spectrum = **5 points, C0..C4, these exact K values** (§5)                        | PARTIAL                                  | the wedge reproduced at ~3 concurrent ws + shared hotspot, ~1.8–1.9× cost there (SP3 §1/§3)                                           | Only C2/C3 is SP3-anchored. The other points interpolate/extrapolate around that single proven operating point. Frozen so conditions can't be added/removed to move the crossover.                                                                                 |
| R6  | `wedge_incident` derived flag threshold = **>1.5× benign-median turns** (§1.1)              | PARTIAL                                  | SP3 §4 _recommended_ a ">1.5× median turns" wedge flag; SP3 itself used divergent-state recovery / abandoned work as the wedge signal | Adopts SP3's own recommended derived-flag rule verbatim; the qualitative part (divergent-state recovery / abandoned committed work) IS SP3-grounded (SP3 §1/§3 observed exactly this). Only the 1.5× cutpoint is a rule, and it is SP3's own suggested cutpoint.   |
| R7  | Crossover reported as a **band with width** when no clean index (§4.3)                      | NO                                       | SP3 measured a large clean separation at one point, not a curve                                                                       | A curve was never measured; refusing to fabricate a precise crossover when the data is a MIXED band is the honest rule.                                                                                                                                            |
| R8  | Headline metrics = median + IQR/CI, **means forbidden** (§4, §6)                            | YES (rule), data PARTIAL                 | SP3 §4 explicitly: variance is bimodal/zero-inflated, size on the proportion not cost-CV                                              | Directly carries SP3's stated discipline; this is SP3-grounded, listed here only for completeness.                                                                                                                                                                 |
| R9  | `work_lost` split into 4 fields + derived `workflow_loss` (§1.1)                            | NO — SP3 used a single VCS-level `work_lost` | SP3 §1 showed jj wedges produce abandoned committed work via change-id divergence — recoverable in principle (op-log) but agent-abandoned | The original single `work_lost` was VCS-centric. A substrate that "loses no bytes" while the agent abandons work or fails to deliver still fails the agent. Split labels each class so a maw "we didn't lose bytes" claim cannot mask a workflow loss. R9 is pure definition; the corresponding threshold is R10. |
| R10 | SG3 `workflow_loss`-rate tolerance = **+0.05 absolute** (§3.1)                              | NO — no layout-A/B data                  | task-success is a stricter bar than coordination-stress events                                                                        | +0.05 ≈ 1 extra failed run in 20 is the noise margin at headline N; below that is jitter, above it the layout meaningfully degrades task success. Stricter than R2's +0.10 because workflow_loss is closer to the user-visible outcome.                            |
| R11 | Block-randomized run order; full version-capture manifest (§6.2, §6.4)                      | NO                                       | SP3 ran few exploratory runs; did not measure time-of-day or version drift                                                            | Hosted-model behavior (caching, routing, rate limits, model updates) is known to drift; without blocked randomization a substrate effect can be confounded with a temporal effect. This is methodology hygiene, not a measurement choice.                          |
| R12 | 20% transcript sample + all wedged runs blind double-coded (§6.3)                           | NO                                       | SP3 §3 measured wedged-run cost ratios but did not code attribution                                                                   | `wasted_turns` attribution is the single most subjective metric in the doc and the most likely site of author-bias leakage. Blind double-coding is the standard mitigation; SP3 did not run it (the spike was author-only). Pre-committed.                         |
| R13 | Discard taxonomy + max-2-retries cap (§8.7)                                                 | PARTIAL                                  | SP3 §2 documented one substrate-induced silent failure mode (`--bare` OAuth break) and mandated discard+rerun                         | The original §8.2 health-gate was binary (discard / count); a single permissive regex can hide substrate-induced failures. The frozen taxonomy + retry cap forces every discard to declare a class so suspicious patterns surface.                                 |
| R14 | Arm 4 `claude-native-worktrees` added (§1.3); min run at C0/C2/C4 if budget-constrained     | NO                                       | SP3 evaluated only `maw` / `git-worktrees-bare` / `jj-workspaces`                                                                     | If maw cannot beat the worktree workflow the same agent tool already ships natively, the publication has not established maw is worth adopting over the default. The original 3-arm framing undertested the modern incumbent; arm 4 closes a fatal external-validity gap. |
| R15 | T1–T5 task classes run at C2 only; T0 at all of C0..C4 (§5.1)                               | NO                                       | SP3 ran one task class (T0-equivalent: code-only shared hotspot)                                                                      | Real agent-worktree pain is broader than file-collision mechanics, but full-cross C × T is over budget. C2 × {T1..T5} captures the differentiating effect at the SP3-anchored point. Frozen so T-classes can't be added/removed to relocate the headline.          |
| R-friction | Friction-axis metrics (§1.1) are **recorded and reported**, NOT a verdict input at v1.0 | NO                                       | SP3 measured no friction-axis baseline                                                                                                | Pre-committing a numeric bar against an unmeasured baseline would be the exact "disguised measurement" failure §7 exists to prevent. The friction axis is the place maw is most likely to win, but a number must wait for the first measured baseline (SG4 / a §9 amendment after T2.2/T2.6). |

**Honest summary:** the only fully SP3-grounded hard bars are
`irrecoverable_lost_work == 0` (Prime Invariant, not a benchmark guess) and
the _existence/direction_ of the jj wedge (SP3 §1 reproduced it). Every
numeric tolerance/target (R1–R5, R10) is a **pre-committed decision rule**,
justified above, frozen to prevent post-hoc tuning — they are honestly
labeled as rules, not disguised as measurements. R11–R15 and R-friction are
methodology choices added in review pass 1; they are also rules, not
measurements, and they are listed here for the same reason.

---

## 8. Fairness controls (carried from SP3 §1/§6, sharpened in review pass 1)

### 8.1 Command cribs (training-data-scarcity and incumbent-affordance controls)

**`jj-workspaces` arm crib (binding — reviewer point P0-5).** The jj arm is
given a maw-equivalent crib so the benchmark isolates the _coordination_
failure, not jj-verb unfamiliarity (agents are git-fluent, jj-scarce — memory
`maw-design-rationale-agent-fluency`; SP3 §1 fairness caveat). The crib
covers, at minimum:

- workspace creation and listing (`jj workspace add` / `list`);
- stale working-copy detection and refresh (`jj workspace update-stale`);
- divergent-change detection and the explicit divergence-resolution policy
  (`jj log -r 'divergent()'`; the policy the agent should apply);
- operation-log inspection (`jj op log`);
- operation-log recovery commands (`jj op restore` / `jj op integrate`) and
  when to AVOID integrating background operations;
- the exact commands the agent may use to inspect op state.

**`maw` arm crib (binding).** Symmetric crib covering: `maw ws create / list`,
`maw ws sync`, `maw ws diff`, `maw ws merge --check / --destroy`,
`maw ws recover`, `maw doctor`, and the conflict-as-data resolution surface
(`maw ws resolve`).

**`git-worktrees-bare` arm crib (binding).** Symmetric crib covering:
`git worktree add / remove / list`, the hand-rolled coordination convention
(`bn-mit2`), and the integration commands.

**`claude-native-worktrees` arm crib (binding).** Symmetric crib covering
the Claude Code worktree surface in use, with the following precision
required by review pass 2:

- **CLI surface:** `claude --worktree` / `-w`. If no worktree name is
  supplied, Claude Code auto-generates one under `.claude/worktrees/<name>`.
- **`worktree.baseRef`** is restricted to one of two literal values:
  - `"fresh"` (default) — branch from the repo's remote default branch
    (`origin/HEAD`), falling back to local `HEAD` when remote/fetch is
    unavailable.
  - `"head"` — branch from local `HEAD`.

  `worktree.baseRef` is **NOT an arbitrary git-ref setting**; a benchmark
  that needs a specific PR is a separate `claude --worktree "#NNNN"` /
  PR-URL surface, not the `baseRef` setting. The chosen value is captured
  in the §6.4 manifest per run.
- **`.worktreeinclude` vs `WorktreeCreate` hook (mutually constraining).**
  If default Claude Code git-worktree creation is used, `.worktreeinclude`
  may copy selected gitignored files (e.g. `.env`, `.env.local`). If a
  `WorktreeCreate` hook is configured, **it replaces default git creation
  entirely and `.worktreeinclude` is NOT processed**; the hook itself must
  perform any env/config copying. The §6.4 manifest records which path was
  used per run (`default git creation + .worktreeinclude` vs
  `custom WorktreeCreate hook`). `WorktreeRemove` is recorded only when
  custom cleanup is configured or invoked.
- **Non-interactive cleanup (`claude -p --worktree`).** SG2's driver is
  `claude -p --output-format json`. **Claude Code does NOT auto-clean the
  worktree at session exit for non-interactive (`-p`) runs** — there is no
  exit prompt. The arm crib and oracle therefore treat cleanup as an
  **explicit step**: either the agent is instructed (via the crib) to
  remove the worktree, or post-measurement harness cleanup is performed
  **separately from** the measured `cleanup_success` / `orphaned_workspace_count`
  metrics. A leftover worktree is NOT a surprising substrate failure under
  `claude -p --worktree`; it is documented behavior. The crib MUST state
  which model the arm uses (agent-driven cleanup vs harness-side cleanup
  with separated metrics) and the §6.4 manifest records it per run.

**All four cribs are equalized in length and detail** (target: same order of
magnitude in word count), reviewed for parity before the first measured run,
and **published as appendices to the report**. No arm receives a maw-specific
affordance, and the headline isolates work-redone / interventions /
divergent-state recoveries, not raw command flailing.

**Optional jj mitigation sub-arm (budget-permitting, NOT required at v1.0).**
A `jj-workspaces-best-practice` sub-arm at C2/C4 only, using the full jj crib
above plus explicit divergence-resolution and op-integration instructions,
to publish alongside the headline jj arm. The publication will state
explicitly that maw did NOT beat a naive jj strawman; the mitigation sub-arm,
if run, shows where jj-with-discipline sits relative to maw on the same
conditions.

### 8.2 Auth health-gate (silent-corruption control)

Every run is health-checked. A run is **discarded and re-run** (counted under
§8.7's `discard_auth` class) if ANY of:

- envelope `is_error == true`, OR
- envelope `subtype` is in the fixed auth-failure set
  `{auth_error, auth_required, login_required}` (preferred over regex), OR
- result text exactly matches one of a fixed published list of auth-failure
  strings (e.g. `"Not logged in"` — the published list is part of the
  benchmark config).

SP3 §2 auth gotcha for context: `--bare` breaks OAuth on the dev host;
scenario repos live under `/tmp` with no `CLAUDE.md`/`AGENTS.md`/`.mcp.json`
so context = task prompt + scenario tree only. **Discarded-run count and
class are reported per §8.7** (transparency on attrition).

### 8.3 Wedge-incidence power sizing

Per §6.1 — sized on the proportion, not cost-CV; N=20/arm in the
loss/crossover regime; every "0/N" result publishes its Wilson 95% upper
bound (§6.1 MDE table).

### 8.4 Adapter parity

Per `bn-mit2`: no arm adapter does extra work that biases metrics; adapter
parity is reviewed before any measured run.

### 8.5 Identical scenario

Same generator (SG1's, via `bn-4qwp`), same fixed seed per (condition,
T-class, replicate) across all arms — measured differences reflect substrate
ergonomics, not task wording or scenario luck.

### 8.6 Driver pinned

Model, `--max-turns`, `--max-budget-usd`, `--permission-mode`, isolation
method fixed per SP3 §2 and identical across every run and arm. The driver
manifest (§6.4) records the effective model the response envelope reports
(in case routing redirects).

**Workspace-trust preflight (binding — review pass 2).** Claude Code's
`--worktree` mode exits with an error until the workspace-trust dialog has
been accepted once per directory, **including with `-p`**. Before any
measured `claude-native-worktrees` run, the benchmark performs the
workspace-trust preflight once per scenario repo / root. Trust-preflight
failures are **harness setup failures** (classified `discard_harness_bug`
per §8.7), NOT a substrate outcome and NOT counted in metrics. Other arms
(`maw`, `git-worktrees-bare`, `jj-workspaces`) do not have this preflight
requirement.

### 8.7 Discard taxonomy and retry cap (frozen — reviewer point P1-6)

Every run that does not produce a counted measurement is classified by one
of the following classes (frozen vocabulary):

| class                             | meaning                                                                                              | counted? |
| --------------------------------- | ---------------------------------------------------------------------------------------------------- | -------- |
| `discard_auth`                    | §8.2 auth-failure health-gate trip                                                                  | no       |
| `discard_harness_bug`             | a defect in the benchmark harness itself (scenario generator, driver wrapper, manifest writer)       | no       |
| `discard_external_service_outage` | provider-side outage during the run (API 5xx, timeout) verified outside the substrate under test    | no       |
| `counted_substrate_failure`       | the substrate under test failed (e.g. maw crashed; jj wedged; git refused) — **counted in metrics**  | yes      |
| `counted_agent_failure`           | the agent failed the task despite the substrate functioning — **counted in metrics**                 | yes      |

**Retry cap:** at most **2 discarded reruns per
`(arm, condition, T-class, replicate)` cell**. Further failures in that cell
are recorded as **`counted_harness_overflow`** entries (a sentinel reported
separately in the publication, never silently dropped). A cell that
overflows is flagged as **unreliable for the publication** and investigated
before the dataset is published.

Per-class discard counts are reported per arm per condition in the
publication. A discard rate that varies systematically by arm is treated as
a fairness flag, not a footnote.

---

## 9. Amendment log

_(Empty at freeze. Any post-`2026-05-24T20:00:00Z` change to a frozen value
MUST be appended here per the §0 freeze clause: ISO-8601 UTC timestamp,
reason, authorizer, the superseded value left readable, and committed
BEFORE the affected run. No entry may retroactively declare a missed target
met. Review-pass edits made BEFORE the first measured run are NOT
amendments — they are pre-acceptance revisions audited in §12.)_

— no amendments —

---

## 10. Acceptance-criteria checklist (spec `bn-2ftq`)

Spec AC: _"Doc committed and timestamped strictly before T2.2/T2.6 runs.
States numeric pass/fail bars for the SG3 layout gate (T3.5) and SG4 targets
(T4.3)."_ Plus task-brief required elements. Plus review-pass-1 additions.

- [x] Explicit ISO-8601 freeze timestamp — §0 (`2026-05-24T20:00:00Z`;
      original freeze `2026-05-17T23:48:34Z`; reset by review pass 1 on
      2026-05-21 then by review pass 2 on 2026-05-24).
- [x] Freeze clause + pre-acceptance review carve-out — §0; §9 log scoped to
      post-run-start changes only.
- [x] Committed strictly before T2.2/T2.6 — verifiable: `bn-1sqo` (T2.2) &
      `bn-3l1f` (T2.6) `open`, both `depends_on bn-2ftq`; no run artifacts
      (§0 precondition).
- [x] Numeric pass/fail bars for SG3 layout gate T3.5 — §3.1 (six metric
      bars + paired-bootstrap CI on efficiency; GO/NO-GO both defined; one
      informational friction subsection).
- [x] Numeric targets for SG4 T4.3 — §3.2 (five targets + renegotiation rule).
- [x] Condition-spectrum definition — §5 (C0..C4, frozen exact K values;
      T0..T5 task-class taxonomy and application schedule frozen in §5.1).
- [x] Non-composite dominance presentation, exact table/figure shape mocked
      — §4.1 (table covering 4 arms across safety/efficiency/friction
      axes), §4.2 (stacked-panel figure, 4 lines), §4.3 (verdict rule using
      paired bootstrap CI, Wilson CI, Hodges–Lehmann, Cliff's δ).
- [x] Explicit double-investment-bias statement + binding to publish
      loss/overkill regime + jj-crib disclosure — §2.
- [x] Fairness controls (per-arm cribs, auth health-gate, wedge-incidence
      power, adapter parity, identical scenario, driver pin) — §8.1–§8.6;
      discard taxonomy + retry cap — §8.7.
- [x] Block-randomized run order — §6.2.
- [x] `wasted_turns` blind double-coding protocol — §6.3.
- [x] Version-capture manifest — §6.4.
- [x] Pre-registered analysis/decision rule per metric — §4.3 + §6 + §8.7.
- [x] Honest ledger of every decision-rule-not-a-number with SP3 basis —
      §7 (R1–R15 + R-friction).
- [x] Review pass 1 disposition — §12.1 / §12.2.
- [x] Review pass 2 disposition — §12.3 / §12.4.

**All acceptance criteria met.**

---

## 11. Implications for downstream bones

- **T2.2 `bn-1sqo` (real-agent driver harness):** must emit, per run, the
  exact §1.1 metric set (efficiency + correctness/safety + friction axes —
  envelope-readable plus transcript-derived plus scenario-oracle-derived;
  the doc no longer overclaims "directly readable"), apply the §8.2 auth
  health-gate (discard+rerun, classify per §8.7), pin the driver per §8.6,
  emit the §6.4 manifest, run cells in the §6.2 block-randomized order,
  and must NOT compute any composite. The harness must reproduce within
  SP3's measured variance.
- **T2.6 `bn-3l1f` (condition-spectrum sweep + crossover):** runs the
  frozen §5 spectrum at §6's N (10 headline / 20 loss-regime), classifies
  each cell by the §4.3 verdict rule, renders the §4.2 stacked-panel
  figure (now 4 lines), reports the crossover as a point or a
  band-with-width, runs T1–T5 at C2 per §5.1, and reports every "0/N"
  with its Wilson upper bound (§6.1). The C0 overkill regime is shipped,
  not clipped (the §2 commitment is T2.6's binding output, consumed by
  the T5.3 `bn-2xfn` publication).
- **T3.5 `bn-1uzn` (layout go/no-go):** decided **mechanically** by the
  §3.1 bars on a paired same-N subset; the §2 binding forbids author
  override of the numeric outcome; a no-go is recorded as an explicit,
  acceptable, non-failure result (v1.0 ships on SG1 alone). Needs §5's
  benign + one mid condition at N≥10/arm/layout; reports paired
  bootstrap CIs on efficiency.
- **T4.3 `bn-1qty` (SG4 re-benchmark):** the §3.2 before/after deltas are
  measured against the frozen baseline T2.2/T2.6 produce under this doc;
  renegotiation only via a §9 amendment committed before the confirming
  re-run, never retroactively-met.
- **T5.3 `bn-2xfn` (publication):** see §2 binding commitments — lead with
  the demonstrated jj wedge + bn-cm63 scar; publish the maw-overkill
  regime vs `claude-native-worktrees`; publish all four cribs as
  appendices; never report a "0 wedge" without its Wilson upper bound;
  never collapse a MIXED band to a point; never compute a composite.

Product / ergonomics suggestions surfaced by review pass 1
(machine-readable workspace manifest, `maw status --json`, `maw doctor` /
`maw repair` expansion, append-only event log, agent crib generation,
environment propagation, safe-cleanup state vocabulary, mergeback queue,
first-class "overkill line" CLI guidance) are **inputs to SG4 / product
roadmap**, NOT pre-registration material. They are captured separately
(see §12 Disposition); they do not appear here because the
pre-registration commits to **measurement**, not to product features.

---

## 12. Review Pass 1 Disposition (2026-05-21)

External review by Sonnet 4.6:
`sg2-benchmark-preregistration.review.1.md` (dated 2026-05-18). Reviewed
and dispositioned by lead on 2026-05-21. The review's executive verdict
was: "strong preregistration; main problem is external validity,"
recommending the modern agent-native worktree workflow be tested as the
load-bearing incumbent.

This disposition is published as part of the audit trail. Edits below were
made BEFORE any measured SG2/SG3/SG4 run, under the pre-acceptance
carve-out in §0 — they are NOT §9 amendments. The §0 freeze timestamp has
been re-stamped from `2026-05-17T23:48:34Z` to `2026-05-21T18:00:00Z` to
reflect the revised, accepted preregistration. §9 remains empty.

### 12.1 Disposition table

Legend: **A** = accepted as written. **A\*** = accepted with adaptation
(noted). **D** = deferred (not pre-registration material; captured
elsewhere). **R** = rejected (rationale given).

| Review item                                                                                  | Pri | Disposition | Where this lives in the revised doc                                                                                                                                                          |
| -------------------------------------------------------------------------------------------- | --- | ----------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Add agent-native worktree baseline arm                                                       | P0  | **A\***     | §1.3 arm 4; §4.1 table 4 columns; §4.2 figure 4 lines; §7 R14; §8.1 crib; §11 T2.6. Adapted: minimum-3-points (C0/C2/C4) budget floor instead of all 5; full 5-point sweep if budget allows. |
| Make C4 concrete (remove `3+` / `max`)                                                       | P0  | **A**       | §5: exact tuple `K_overlap=8/8, K_concurrency=4, K_rounds=8, burst`; battery size N=8 frozen; all K_overlap stated as `n/8` not `~%`.                                                       |
| Add randomization / blocking                                                                 | P0  | **A**       | §6.2 block-randomized schedule; §6.4 version-capture manifest with `arm_order_index`; §7 R11.                                                                                                |
| Split `work_lost` into integrated/orphaned/irrecoverable                                     | P0  | **A**       | §1.1 metric split (4 fields); §3.1 hard bar = `irrecoverable_lost_work == 0`; new `workflow_loss` derived metric with +0.05 tolerance (R10); §3.2 mirror; §7 R9 + R10.                       |
| Best-effort jj protocol (not only crib)                                                      | P0  | **A\***     | §8.1 expanded jj crib (workspace ops, stale WC, divergent detection, op-log inspection/recovery, when NOT to integrate); optional `jj-workspaces-best-practice` sub-arm named but not required at v1.0 (budget-permitting). |
| Power / MDE table; Wilson upper-bound reporting                                              | P1  | **A**       | §6.1 MDE table + binding reporting rule ("0/20" must publish Wilson CI); §4.1 every proportion cell carries Wilson CI; §4.3 verdict uses Wilson CI bounds on rate comparisons.               |
| Paired bootstrap / Cliff's δ replacing "IQRs do not overlap"                                 | P1  | **A**       | §4.3 dominance rule rewritten on paired bootstrap CI + median ratio; mandatory Hodges–Lehmann + Cliff's δ supplementary reporting.                                                          |
| Blind double-code `wasted_turns`                                                             | P1  | **A**       | §6.3 coding protocol (20% sample + all wedged runs, blind where feasible, adjudication before aggregation, inter-rater agreement published); §7 R12.                                        |
| Task-class taxonomy (T0..T5) beyond shared-file hotspot                                      | P1  | **A\***     | §5.1 frozen T0..T5 with application schedule: T0 at all of C0..C4; T1..T5 at C2 only (budget-bounded v1.0 commitment); §7 R15. Expansion beyond C2 does not require an amendment.            |
| Setup/cleanup friction metrics (non-speed)                                                   | P1  | **A\***     | §1.1 friction axis (7 metrics); §3.1 informational subsection (v1.0 records and reports but does NOT bar); §4.1 separate friction-axis rows. Adapted: SP3 measured no friction baseline, so a numeric bar would be a disguised measurement (§7 R-friction). SG4 / a §9 amendment may add bars after T2.2/T2.6 establishes a baseline. |
| Reclassify discarded runs (taxonomy + retry cap)                                             | P1  | **A**       | §8.7 5-class taxonomy + max-2-retries-per-cell + `counted_harness_overflow` sentinel; §6.4 manifest carries `discard_class`/`discard_reason`; §7 R13.                                       |
| Fix "directly readable" wording                                                              | P2  | **A**       | §1.1 lede rewritten ("driver records the envelope plus transcript / tool events; the benchmark derives…").                                                                                  |
| Fix malformed markdown in §5                                                                 | P2  | **A**       | §5 markup corrected (`_design choices_` properly formed; `K_overlap` not `K*overlap`).                                                                                                       |
| Replace "all three arms identical" wording                                                   | P2  | **A**       | §5 K_overlap explanation rewritten ("a no-overlap scenario removes the main coordination failure mode and primarily measures setup/lifecycle friction, not contention"); also no longer "three arms" — now four. |
| Suggested §9 entries A1/A2/A3                                                                | —   | **R**       | Pre-acceptance review-driven edits are NOT §9 amendments per the §0 carve-out. The review's intent (add arm 4 / concretize C4 / block-randomize) is fully incorporated as pre-acceptance edits, with §7 R11/R14 and §12 capturing the audit trail. §9 remains empty at freeze; future post-freeze changes will populate it. |
| Product-implications (machine-readable manifest, `maw status --json`, `maw doctor`/`repair`, event log, agent crib generation, `.mawinclude`, safe-cleanup states, mergeback queue, first-class overkill-line CLI guidance) | —   | **D**       | These are product / ergonomics inputs, not pre-registration material. To be captured against SG4 (`bn-2j45`) and the product-roadmap bones via bone comments (lead will file the cross-references). The pre-reg deliberately commits only to **measurement**; carrying product features into the pre-reg would bloat the trust artifact and couple the freeze to a moving feature surface. The "first-class overkill line" *publication* requirement is already captured in §2 (publication explicitly publishes the overkill regime); the *CLI* version is product. |

### 12.2 Methodology meta-note

Review pass 1 was conducted before any measured run. Its scope was
methodology and external validity, not data. Three properties of this
disposition matter:

1. **No bar was changed in response to data** (no data exists). All bar
   changes are responses to _pre-data_ methodological critique. The
   asymmetry rule (post-data tightening = bias) is intact.
2. **Every accepted change is recorded** in either the relevant section or
   in the §7 honesty ledger (R9–R15 + R-friction are the review-derived
   rules). Future reviewers can trace any frozen value back to its
   rationale.
3. **The §0 freeze timestamp was re-stamped** because the doc was
   substantively revised. The original 2026-05-17 freeze stands as the
   timestamp of the _first draft_; the 2026-05-21 freeze is what binds the
   benchmark. This is the only time the pre-acceptance carve-out applies;
   subsequent post-run-start changes are §9 amendments.

If a review pass 2 occurs before runs start, it follows the same protocol
(append §12.x for that pass; re-stamp §0 if anything frozen changes; do
not touch §9).

### 12.3 Review Pass 2 Disposition (2026-05-24)

External review by Sonnet 4.6:
`sg2-benchmark-preregistration.review.2.md` (dated 2026-05-24). Reviewed
and dispositioned by lead on 2026-05-24. The review's executive verdict
was: **"ACCEPT after small pre-run text fixes."** The reviewer explicitly
declined to reopen pass 1's decisions and confined pass 2 to the three
focused asks plus consequent findings: factual verification of the
`claude-native-worktrees` surface, structural pressure-test of the
friction-axis "informational-only" call, and statistical correctness of
the new bootstrap/Wilson/Cliff's δ machinery.

This disposition is published as part of the audit trail. Edits below
were made BEFORE any measured SG2/SG3/SG4 run, under the pre-acceptance
carve-out in §0 — they are NOT §9 amendments. The §0 freeze timestamp
has been re-stamped from `2026-05-21T18:00:00Z` to `2026-05-24T20:00:00Z`
to reflect the revised, accepted preregistration. §9 remains empty.

| Review item                                                                                  | Disposition | Where this lives in the revised doc                                                                                                                                                              |
| -------------------------------------------------------------------------------------------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Factual: `worktree.baseRef` only accepts `"fresh"` or `"head"`                               | **A**       | §1.3 arm 4 (sketched + cross-ref); §8.1 arm 4 crib (full constraint + manifest capture); not an arbitrary git-ref knob is stated explicitly.                                                     |
| Factual: `.worktreeinclude` is NOT processed when `WorktreeCreate` hook replaces creation    | **A**       | §8.1 arm 4 crib: mutual constraint stated explicitly; manifest records which path was used (default git creation + `.worktreeinclude` vs custom hook). `WorktreeRemove` recorded when applicable. |
| Factual: `claude -p --worktree` does NOT auto-clean                                          | **A**       | §8.1 arm 4 crib: cleanup is an explicit step under non-interactive `-p`; either agent-driven cleanup or harness-side cleanup separated from measured `cleanup_success` / `orphaned_workspace_count`. |
| Factual: workspace-trust preflight required                                                  | **A**       | §8.6: trust-preflight performed once per scenario repo before measured arm-4 runs; preflight failures classified `discard_harness_bug` (§8.7), not substrate outcomes.                            |
| Structural: keep friction axis report-but-don't-bar for v1.0                                 | **A**       | §3.1 retains the informational subsection; added explicit "SG3 gate is non-regression, not direct-friction" clarification.                                                                       |
| Structural: permit harness-only validation pilots, prohibit bar-setting pilots               | **A**       | §3.1 new "Pilot rule" frozen subsection: harness sanity-only pilots permitted; data excluded from analysis; using pilot data to set bars or support publication claims = frozen-clause violation. |
| Stats: prose admit ×1.15 is a materiality margin (statistically real sub-margin regressions pass through) | **A** | §3.1 efficiency-bar rows: rewritten with explicit "no-go iff CI excludes 0 on the worse side **AND** median ratio exceeds ×1.15; sub-margin regressions intentionally permitted." §4.3 mirrors. |
| Stats: name the bootstrap resampling unit                                                    | **A**       | §6.2: bootstrap unit explicitly named as the `(condition, T-class, seed, replicate)` block; resample matched units; warn against per-arm independent resampling.                                  |
| Stats: §4.3 rate-comparison rule misnamed as "Wilson CI test"                                | **A** (minimal-wording fix) | §4.3: relabelled as a **pre-registered material-gap rule** combining Wilson intervals (for display) with a practical-effect override (`+0.10` point-estimate gap); explicitly called out as NOT a Wilson statistical-significance test. Chose the wording fix over swapping to a paired-binary-bootstrap rate rule; the existing practical-effect intent was correct and unchanged. |
| Stats: Cliff's δ pairing                                                                     | **A** (option A — label as unpaired/descriptive) | §4.3: Cliff's δ explicitly labeled "descriptive / unpaired stochastic-dominance"; report MUST NOT present it as paired. The paired effect-size role is filled by Hodges–Lehmann (already supplementary). Option A chosen over option B (matched-pairs rank-biserial) for familiarity and because Hodges–Lehmann already covers the paired side. |
| Stats: SG4 CI direction explicit (excludes 0 in improvement direction)                       | **A**       | §3.2: `turns_to_done` hostile-reduction bar rewritten with explicit direction `median(after)/median(before) ≤ 0.85` AND paired bootstrap 95% CI for `(after − before)` excludes 0 on the improvement (negative) side; equivalent positive-side formulation noted.                                                                            |

### 12.4 Pass-2 methodology meta-note

All eight pass-2 items were accepted. None of them altered a frozen bar's
**intent** — they tightened prose so readers cannot misread practical-effect
rules as statistical-significance tests, pinned three factual claims about
the `claude-native-worktrees` surface to the current Claude Code
documentation, and added explicit direction / pairing / pilot discipline
that pass 1 left implicit. The honesty ledger (§7) did not gain new R-rules
because no new pre-committed value or threshold was introduced — every
change here clarifies an already-frozen rule or adds operational discipline
(pilot exclusion, trust preflight) consistent with the §0 freeze clause.

Per the reviewer's explicit recommendation ("no new review pass needed
unless the changes alter the intended rules rather than clarify them"), and
with all eight items dispositioned as clarifications rather than rule
changes, **the lead does not plan a review pass 3 on this document**. The
document is **accepted** at this freeze, pending lead's choice on
when/whether to merge to `main` (which clears the §0 pre-acceptance
carve-out and locks §9 as the only further-change channel).
