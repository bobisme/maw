# SG3 Layout-Eval Subset Pre-Registration (bn-iux4)

**Parent:** SG3 `bn-2yh1`. **Authoritative spec:** `bn show bn-iux4`.
**Downstream consumer:** T3.5 `bn-1uzn` (the run + go/no-go decision).
**Upstream pins (verbatim, never re-derived here):**

- `notes/sg2-benchmark-preregistration.md` — the SG2 master pre-reg (T2.7,
  `bn-2ftq`, frozen `2026-05-24T20:00:00Z`).
- `notes/sg2-metric-definitions.md` — the metric vocabulary (T2.4 + T2.5
  amendments).
- `notes/sg2-friction-list.md` + `notes/sg2-friction-list-handoff.md` — the
  SG4-input shape carried forward (T2.8).
- `notes/agent-benchmark-feasibility.md` — SP3 statistical framework (`bn-2ixm`).
- `crates/maw-bench-sweep/src/grid.rs` — the frozen §5/§5.1 spectrum +
  `derive_seed` mechanism the subset RUN (T3.5) will use.

This document is the **load-bearing trust artifact** for the SG3 layout gate.
Its purpose is to remove every post-hoc degree of freedom from the SG3
layout-eval _before_ any SG3 layout-implementation commit lands — so the bar
the layout must clear cannot be silently softened once layout data exists,
and the §2 SG2 double-investment bias cannot reach into SG3.

---

## §0 Pre-registration metadata (FREEZE)

**Freeze timestamp (ISO-8601 UTC):** `2026-05-25T00:00:00Z`.

**Freeze clause.** No subset definition, metric carry-forward, regression
rule, MDE, direction-of-test, failure-mode statement, or CI-gate rule in
this document may be changed after the freeze timestamp **except by a
logged, justified amendment** (see §7). An amendment is valid only if it
is (a) appended to §7 with its own ISO-8601 UTC timestamp, (b) states the
reason and who/what authorized it, (c) is committed _before_ the T3.5
measured run it affects, (d) never deletes or rewrites a frozen value, and
(e) never tightens or loosens a bar in response to seen data. Softening
the bar after T3.5 produces data is the exact failure this clause exists
to prevent; post-data amendments are permitted only to _report a target
as missed and renegotiated_, never to retroactively declare it met
(matching the T4.3 / §0 discipline in `sg2-benchmark-preregistration.md`).

**Pre-acceptance review carve-out.** Edits made BEFORE T3.5's first
measured run, in response to a reviewer pass on _this_ doc, are NOT §7
amendments — they are pre-acceptance revisions that re-stamp the freeze
timestamp and are audited in §7 (Disposition). Once T3.5's first
measured run starts, this carve-out closes and §7 is the only legal
modification channel.

**Pre-run precondition (verifiable).** This doc is committed strictly
before any SG3 layout-implementation commit lands. Evidence:

- Bone `bn-iux4` is `doing` at freeze; the SG3 implementation children
  (`bn-2sw3` T3.2 Implement, `bn-3kkl` T3.3 Migration, `bn-1jqo` T3.4
  guardrail) all depend on either this bone or T2.7 / SP4 — see the
  bones graph.
- The canonical layout-implementation file
  `crates/maw-cli/src/workspace/create.rs` has `mtime`
  `2026-05-16` at freeze (well before this doc's commit). The
  §6 CI gate enforces "doc commit-time strictly less than any future
  modification commit-time on that file" — once the layout work starts
  modifying `create.rs`, this doc must already be in `main`.
- SG2 baseline pre-condition: this bone's spec (`bn show bn-iux4`)
  explicitly requires SG2 (`bn-2jwi`) to have produced its baseline
  benchmark on the current `ws/` layout BEFORE T3.5 runs the subset.
  Without that baseline there is no "before" to compare against.

**Authoritative bones:**

| bone     | role                                                       |
| -------- | ---------------------------------------------------------- |
| bn-iux4  | THIS doc (subset pre-reg)                                  |
| bn-2yh1  | SG3 parent (layout decoupling)                             |
| bn-2ftq  | T2.7 — SG2 master pre-reg (the surface this subsets)       |
| bn-1uzn  | T3.5 — the layout-eval run + go/no-go decision (consumer)  |
| bn-2sw3  | T3.2 — layout implementation (gated by this doc)           |
| bn-3kkl  | T3.3 — v2→new-layout migration                             |
| bn-1jqo  | T3.4 — guardrail relocation                                |
| bn-2jwi  | SG2 parent (the baseline this gate compares against)       |
| bn-2kgu  | SP5 — directional spike (informs strategy; NOT this bar)   |

**Harness commit pin (at freeze):** `cd055004120cec4ceb7fb5e3f9b6d7d9e7899e1a`
(the workspace base epoch this doc was authored against). T3.5 will pin its
own `benchmark_harness_commit` per `sg2-benchmark-preregistration.md` §6.4;
that pinned SHA must be ≥ this freeze commit (no backward harness
substitution).

---

## §1 The SUBSET (smaller-N projection of T2.7's grid)

**Inheritance rule (binding).** This subset inherits T2.7 verbatim where
possible. The condition spectrum, T-class taxonomy, derive_seed mechanism,
manifest schema, bootstrap unit, attribution rules, blind double-coding
protocol, and discard taxonomy are NOT re-frozen here — they are imported
by reference from `sg2-benchmark-preregistration.md`. If a §7 SG2 amendment
ever tightens an imported rule, this doc inherits the tighter rule
automatically (subset can never be _looser_ than parent).

### §1.1 Cells (which (condition, T-class) combinations run)

The subset is **two cells × one T-class** (T2.7 §3.1's "benign + one
mid-spectrum condition" instruction, made concrete):

| cell   | condition (from T2.7 §5)              | T-class | rationale                                                                                                                                                                       |
| ------ | ------------------------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| SUB-A  | **C0** (`benign`, `0/8`, 1, 1)        | T0      | The maw-overkill regime per T2.7 §5 — captures setup / lifecycle friction with NO contention. The layout change touches setup paths most directly; this is where a layout regression is most likely to surface. |
| SUB-B  | **C2** (`moderate`, `4/8`, 3, 5)      | T0      | The SP3-anchored mid-spectrum wedge-trigger point (per T2.7 §5). The only point in the spectrum where SP3 measured a real coordination effect; the layout change MUST NOT degrade behavior here. |

**What is NOT in the subset (and why):**

- **C1 / C3 / C4** are excluded. The layout change moves _where files
  live_, not _how coordination resolves_; the C3/C4 hostile end is where
  the substrate-coordination effect dominates and is already proven
  layout-invariant by SP4 (`notes/layout-engine-impact.md` — 12/13
  trivial-relocation, 0 rewrites inside merge/build/collect/diff3 engine).
  Running C3/C4 here would burn budget without changing the verdict.
  C1 is excluded to keep the subset small; if SUB-A and SUB-B both pass,
  C1 has no power to flip the gate; if either fails, the gate is no-go
  regardless of C1.
- **T1–T5** are excluded. T2.7 §5.1 already runs T1–T5 at C2 only as the
  budget-bounded v1.0 commitment; layering them into the SG3 subset would
  double the run cost without independent signal on layout (T1–T5 stress
  env / install / mergeback / rebase / cleanup — all of which are
  layout-touching paths, but the same _layout_ touches them at C0 and C2
  already through the §1.2 metrics). If a T1–T5 layout regression is
  suspected post-T3.5, it surfaces in the SG4 friction list (the same
  classifier runs against T3.5's transcripts; see §1.4).

### §1.2 Substrates (what runs at each cell)

**Two substrates, paired (the gate is maw-vs-maw, NOT maw-vs-rivals):**

| substrate id           | what                                                                                       |
| ---------------------- | ------------------------------------------------------------------------------------------ |
| `maw@old-layout`       | maw on the current `ws/` layout (v2 bare-repo). The "before" arm.                          |
| `maw@new-layout`       | maw on the proposed SG3 layout (normal root checkout + hidden `.maw/worktrees/`). The "after" arm. |

**Rivals out of subset.** `git-worktrees-bare`, `claude-native-worktrees`,
and `jj-workspaces` are NOT in the subset. T2.7 §3.1 says verbatim: "the
jj and `claude-native-worktrees` arms are not required for this gate since
it tests _maw's own_ layout". `git-worktrees-bare` is excluded for the
same reason. The subset measures whether the layout change makes agents
worse at using **maw**, not whether maw is still better than rivals
post-layout (the latter is the SG2 master benchmark T2.6 will deliver).

**Substrate-adapter requirement.** T2.3's substrate adapter framework
must expose `maw@new-layout` as a first-class substrate id by the time
T3.5 runs. This is a T3.2 / T3.3 implementation deliverable; it is NOT
a freeze blocker for this doc (this doc registers the bar, not the
implementation), but T3.5 cannot start without it. The subset's
`derive_seed` calls use `arm == "maw@new-layout"` and `arm ==
"maw@old-layout"`; the harness-commit pin (§0) records which adapter
revision was used per run.

### §1.3 Sample size N per cell per substrate, with MDE justification

**Per-cell N (frozen):**

| cell  | substrate          | N (runs) | rationale                                                                                                                                                                                                                                       |
| ----- | ------------------ | -------: | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| SUB-A | `maw@old-layout`   | **20**   | C0 is the **loss / overkill regime** per T2.7 §6.1 ("N=20 runs/arm/(condition,T-class) … tight CIs are required exactly where maw loses or is overkill"). The layout-eval subset inherits loss-regime N because C0 is precisely the candidate site of a layout regression (lifecycle / setup friction). |
| SUB-A | `maw@new-layout`   | **20**   | paired by `derive_seed` against `maw@old-layout`                                                                                                                                                                                                |
| SUB-B | `maw@old-layout`   | **10**   | C2 is the headline-regime N per T2.7 §6.1 ("Headline N = 10 runs/arm/(condition,T-class)"). C2 is the SP3-anchored wedge trigger; a layout-induced regression here is large by construction (a layout change should not move coordination behavior at all), so the headline N suffices. |
| SUB-B | `maw@new-layout`   | **10**   | paired by `derive_seed` against `maw@old-layout`                                                                                                                                                                                                |

**Subset total: 60 measured runs** (20 + 20 + 10 + 10). Cost envelope
per SP3 §3: ≈ $0.08 happy-path per run, ≈ $0.14 wedge-path → **subset
campaign cost ≈ $5–8** at SP3 rates. Comfortably below the SG2 master
sweep cost envelope, so the subset is _additionally_ runnable, not a
substitute.

**MDE (minimum-detectable-effect) the chosen N actually has power for.**
The §3 regression rules below are stated in two forms — proportion-based
rates (binary metrics) and median-ratio efficiency (continuous metrics).
MDE for each, computed from SP3 §3 / §4:

#### Proportion-rate MDE (wedge_incident, workflow_loss)

Per T2.7 §6.1 / SP3 §4, power is sized on the **proportion**, not
cost-CV. At N = 20 per arm per cell (loss-regime), with a baseline
proportion `p₀ = 0.05` (one wedged run in 20, a realistic SG2 baseline
ballpark) and α = 0.05, **80% power detects an absolute difference of
≈ +0.20**. This is the subset's _detectable_ effect at SUB-A. The §3
regression band (+0.10 for `wedge_incident`, +0.05 for `workflow_loss`,
both ABSOLUTE, inherited from T2.7 §3.1 / §7 R2 / R10) is therefore
**inside the MDE envelope** at SUB-A — meaning a subset failure that
trips those bands is real signal at this N; a subset _pass_ at these
bars is consistent with up to ~0.20 true regression (a known
limitation, called out explicitly per the T2.7 §6.1 Wilson-upper-bound
discipline; this doc inherits that discipline, see §3.4 below).

At N = 10 per arm at SUB-B, with a baseline `p₀ ≈ 0.10` (C2 is the
wedge-trigger point; SP3 §1 measured the wedge mechanism here), 80%
power detects ≈ +0.30 absolute. The SUB-B regression bands are
deliberately the SAME as SUB-A (+0.10 / +0.05) — meaning at C2 the
subset can FAIL the gate with high signal but can only PASS with the
explicit "consistent with up to ~0.30 true regression" caveat. This is
the bias-against-the-layout-change discipline (§3.5) in action: a
borderline result at SUB-B does NOT pass; the burden is on the layout
change to demonstrate clear separation, and SUB-A's loss-regime N is
what carries the real power.

#### Median-ratio efficiency MDE (turns_to_done, tool_calls)

Per SP3 §3 / §4 and T2.7 §7 R1, the relevant noise floor is the
happy-path turns CV = 9.5% (cost CV = 4.8%). At N = 10/20 with a
paired design and α = 0.05 / 80% power, the **paired bootstrap 95% CI
on the median ratio `median(new) / median(old)` excludes 0 (i.e. has a
detectable signal) for ratios outside ≈ ±10%**. The T2.7 §3.1
materiality margin is ×1.15 (≈ +15% deterioration); this is set
deliberately ABOVE the MDE so that a statistically real but
practically tiny regression (sub-margin) is intentionally permitted
through (T2.7 §3.1 / §4.3 / pass-2 §3.2 wording). This subset
inherits the ×1.15 materiality margin verbatim; it is NOT softened
here.

**Detectable-effects honest disclosure (per T2.7 §6.1 R8 discipline):**

| metric              | MDE at SUB-A (N=20/arm) | MDE at SUB-B (N=10/arm) | regression band (this doc §3) |
| ------------------- | ----------------------: | ----------------------: | ----------------------------- |
| `irrecoverable_lost_work` rate | any > 0 trips (hard bar) | any > 0 trips (hard bar) | == 0 (Prime-Invariant hard bar) |
| `workflow_loss` rate           | ≈ +0.20 abs              | ≈ +0.30 abs              | +0.05 abs (inherited T2.7)   |
| `wedge_incident` rate          | ≈ +0.20 abs              | ≈ +0.30 abs              | +0.10 abs (inherited T2.7)   |
| median `turns_to_done`         | ≈ ±10% (paired CI)       | ≈ ±15% (paired CI)       | ×1.15 (inherited T2.7)       |
| median `tool_calls`            | ≈ ±10% (paired CI)       | ≈ ±15% (paired CI)       | ×1.15 (inherited T2.7)       |
| `interventions` total          | paired diff sign         | paired diff sign         | no net increase (inherited)  |

The bars and the MDE are NOT the same thing; the bars are the
pre-committed _decision rule_ and the MDE is what the data CAN see at
this N. Reporting MUST publish both, per T2.7 §6.1.

### §1.4 Friction-axis inheritance (the SG4 input shape)

T2.8 (`bn-u9iy`) produces a `FrictionList` (schema v1) from BenchRun
artifacts — see `notes/sg2-friction-list-handoff.md`. The same classifier
(`attribute_tool_call` + the two-call stale-read window) MUST run against
T3.5's subset artifacts so the friction signal on `maw@new-layout` is
observable in the same units as the SG2 baseline.

**Friction-rule for the subset (frozen):**

- T3.5 emits the same BenchRun schema v2 records the SG2 master sweep
  emits.
- T3.5 invokes `just sg2-friction-list <artifact-dir>` against the subset
  artifacts to produce a layout-eval-flavored FrictionList JSON.
- The `recommended_fix_class` hints are inherited; the
  `total_unattributed_wasted_turns` bucket MUST be surfaced (T2.8 hard
  rule).
- If a NEW friction cluster appears in the new-layout top-3 that was
  NOT in the old-layout top-3 (regardless of cost), this is flagged in
  T3.5's go/no-go writeup as a "new-layout-introduced friction" finding.
  It does NOT _alone_ trip the §3 gate (it is informational per T2.7
  §3.1 friction-axis rule), but it MUST appear in the publication
  alongside the layout decision.

This rule ensures the subset doesn't silently lose the friction signal
that SG4 will later harden against.

---

## §2 Metrics carry-forward (verbatim from T2.4, with T2.5 amendments)

The metrics this subset measures are exactly the T2.7 §3.1 SG3-gate
metric set, sourced per `notes/sg2-metric-definitions.md`:

### §2.1 Correctness / safety axis (higher-is-worse; 0 is the bar)

| metric                       | source (verbatim T2.4)                                                                                                                                                | axis        |
| ---------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------- |
| `irrecoverable_lost_work`    | scenario oracle: expected work is not reachable by normal VCS / maw recovery mechanisms                                                                               | correctness |
| `workflow_loss`              | derived: `!deliverable_integrated OR recoverable_orphaned_work` (T2.4)                                                                                                | correctness |
| `wedge_incident`             | derived boolean: a divergent-state recovery, abandoned committed work, OR `turns_to_done > 1.5 × arm-median-of-the-benign-condition` (T2.7 §1.1 / §7 R6, SP3-recommended) | correctness |
| `interventions`              | events where the agent abandons/discards committed work or escalates out of the task to recover                                                                       | correctness |

`work_lost_events` (the T2.4 BenchRun v1 schema rollup) IS the
substrate-agnostic surface; if T3.5 runs after the T2.6 schema-v3 split
ships, the per-named-metric rows above are read directly. If T3.5 runs
on v2, `work_lost_events == 0` is the v2-equivalent hard-bar precondition
and the per-named split is reconstructed from the scenario oracle per
the v3 migration plan in `sg2-metric-definitions.md`.

### §2.2 Efficiency axis (lower-is-better; NEVER safety)

| metric                  | source (verbatim T2.4)                                                                                                                              | axis       |
| ----------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- | ---------- |
| `turns_to_done`         | `BenchRun.total_turns` when verdict==Success; otherwise `Infinite` sentinel                                                                         | efficiency |
| `tool_calls_total`      | `BenchRun.total_tool_calls` (sum across transcript turns; includes errored calls)                                                                   | efficiency |
| `work_redone_turns` / `wasted_turns` | T2.4 heuristic + T2.5 attribution-driven count for the maw arm (`MawVerbAttribution` clusters; conservative — unattributed bucket surfaced)                | efficiency |

`cost_usd` and `wall_duration_ms` are RECORDED in the manifest per T2.4
but are NOT verdict inputs for this gate (T2.7 §1.2 speed-not-measured
rule; `duration_ms` CV 28.4% is noise per T2.4).

### §2.3 Friction axis (informational; NOT a verdict input)

Per T2.7 §3.1 friction subsection and §7 R-friction, the friction-axis
metrics from T2.4 (workspace_setup_tool_calls,
first_correct_workspace_tool_call_index, workspace_discovery_failures,
mergeback_tool_calls, cleanup_success, orphaned_workspace_count,
doctor_repair_required) are RECORDED and REPORTED but are NOT verdict
inputs for the SG3 gate at v1.0. T3.5's writeup MUST include the friction
table per the §4.1 cell format; the friction signal feeds SG4, not the
go/no-go.

### §2.4 Per-verb attribution (T2.5)

T2.5 ships `MawVerbAttribution` with 12 named cluster variants (see
`sg2-metric-definitions.md` "T2.5 update"). T3.5 inherits the
classifier verbatim; `maw@new-layout` runs are attributed to the SAME
cluster vocabulary so per-cluster cost is comparable old↔new. If the
new layout introduces a verb pattern not in the existing vocabulary,
the cost shows up in `total_unattributed_wasted_turns` and is flagged
for the human double-coding pass per T2.7 §6.3.

---

## §3 Regression rules (the FROZEN per-metric bars)

This section is the load-bearing decision rule. Each bar is **inherited
from T2.7 §3.1** verbatim. The subset does NOT introduce new bars; it
makes the T2.7-pre-registered bars MEASURABLE at the chosen subset N.

### §3.1 The per-metric bars (frozen, inherited from T2.7 §3.1)

**GO (merge SG3 into v1.0) iff ALL of the following hold across BOTH
SUB-A and SUB-B:**

| #  | metric                            | bar                                                                                                                                                                                                                                                            |
| -- | --------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R1 | `irrecoverable_lost_work` (new)   | **exactly 0** across all subset runs (hard; one occurrence = NO-GO) — the Prime Invariant bar. SP3-grounded.                                                                                                                                                   |
| R2 | `workflow_loss` rate (new)        | new-layout rate ≤ current-layout rate **+ 0.05 absolute** (T2.7 §3.1 / §7 R10). The agent must not fail the task more often under the new layout.                                                                                                              |
| R3 | `wedge_incident` rate             | new-layout rate ≤ current-layout rate **+ 0.10 absolute** (T2.7 §3.1 / §7 R2).                                                                                                                                                                                |
| R4 | median `turns_to_done` (paired)   | **NO-GO iff** the paired bootstrap 95% CI for the difference `(new − current)` excludes 0 on the worse (positive) side **AND** the median ratio `median(new) / median(current)` exceeds **×1.15** (T2.7 §3.1 / §7 R1 / pass-2 §3.2). Both conditions must hold for a NO-GO. |
| R5 | median `tool_calls_total` (paired) | same rule as R4: NO-GO iff paired bootstrap CI for `(new − current)` excludes 0 on the worse side **AND** median ratio exceeds **×1.15**.                                                                                                                      |
| R6 | `interventions` total             | new-layout total ≤ current-layout total (no net increase in human-intervention events; inherited T2.7 §3.1).                                                                                                                                                   |

### §3.2 Dominance-axis rule (the crossover-shift bar)

T2.7 §4.3 defines a per-(maw, arm-X) dominance verdict per cell. SG3's
subset does NOT include rivals (§1.2), so the crossover-axis rule is
adapted:

**Dominance-axis rule (frozen, subset-specific):** the layout change
MUST NOT shift the C0→C2 "maw is overkill → maw dominates / X loses"
band that T2.6 measures on the SG2 master sweep. Operationally, the
subset enforces this _indirectly_ by requiring R4/R5 to hold at BOTH
SUB-A (C0) and SUB-B (C2) — a layout change that flattens the C0
overkill region while degrading C2 would shift the crossover into the
hostile direction (more cells where maw is overkill, fewer where maw
dominates). Because R4/R5 use a paired bootstrap at the cell level and
the materiality margin is the same ×1.15 at both cells, the only way
the dominance band can shift adversely WITHOUT tripping R4/R5 is by a
sub-materiality shift on both ends simultaneously — and that is
exactly the case T2.7 §7 R1 "intentionally permits through" as
sub-margin noise.

If T3.5's writeup observes that R4/R5 pass at BOTH cells but the
direction of the (new − old) median differences is _adverse_ at both
cells, this MUST be reported in the writeup as a "directional adverse
signal below materiality threshold" finding. It does NOT trip the gate
(per pre-committed materiality), but a §7 future amendment may
re-examine the ×1.15 margin if multiple SG3 children produce
consistent directional adverse signals.

### §3.3 MDE the chosen N actually has power for

See §1.3 above for the per-bar MDE table. Key facts re-stated here for
unambiguous reference:

- **R1 (`irrecoverable_lost_work` == 0)** is a hard bar; N is
  irrelevant — any single occurrence trips it.
- **R2 (`workflow_loss` +0.05)** is set BELOW the subset MDE at SUB-A
  (~+0.20). The bar can be tripped only by a regression substantially
  larger than the bar value. A subset PASS at R2 is consistent with up
  to ~+0.20 true regression at SUB-A and ~+0.30 at SUB-B (Wilson
  upper-bound discipline applies per T2.7 §6.1). This is intentional:
  the bar is the pre-committed _decision rule_ (T2.7 §7 R10); the MDE
  is the data-resolution limit. The subset can refuse to ship a layout
  that fails the bar; the subset cannot PROVE no-regression below the
  MDE — that would be a disguised measurement.
- **R3 (`wedge_incident` +0.10)** — same shape as R2; bar inside MDE.
- **R4 / R5 (×1.15 paired-CI gates)** — the materiality margin SITS
  ABOVE the subset MDE (~10–15%), so a sub-materiality regression is
  intentionally permitted through. T2.7 §3.1 / §7 R1 explains why.
- **R6 (`interventions` no-net-increase)** — directional / sign-based;
  N=10/20 paired suffices to see the direction.

### §3.4 Wilson-upper-bound reporting discipline (inherited from T2.7 §6.1)

Every "0 observed" result in the subset MUST publish its Wilson 95%
upper bound, not "0". For SUB-A (N=20): a 0/20 wedge result is
consistent with up to ~16.1% true rate; for SUB-B (N=10): a 0/10
result is consistent with up to ~27.8%. T3.5's writeup MUST phrase
zero-event findings as e.g. `0/20 observed, Wilson 95% CI [0.000,
0.161]` — NEVER as `new-layout wedge rate = 0`. This carries the T2.7
§6.1 honesty discipline into the subset.

### §3.5 Pre-registered direction (BIASED AGAINST THE LAYOUT CHANGE)

This is the discipline that prevents a borderline subset result from
drifting toward "ship it" via human optimism.

**The test is superiority-or-equivalence, with the burden on the
layout change to demonstrate non-inferiority. Ties go to the old
layout.**

Concretely:

- The §3.1 inequalities are written `new ≤ old + margin`. A point
  estimate that satisfies the inequality with `new == old` PASSES; a
  point estimate that satisfies it with `new < old` (the layout
  IMPROVED the metric) also passes, and is reported as a positive
  finding.
- For R4 / R5 (median-ratio gates), `median(new) / median(old) == 1.00`
  PASSES; `< 1.00` PASSES with a positive-finding note.
- For R1 (hard bar), there is no equivalence band; `new == 0` PASSES,
  `new > 0` is a NO-GO.
- For R2 / R3 (rate bars), the inequality includes the materiality
  margin; `new − old ≤ margin` PASSES even when the point estimate is
  adverse.
- **The borderline case** — where any §3.1 bar's point estimate sits
  exactly AT the margin — RESOLVES TO NO-GO. This is the "ties go to
  the old layout" tiebreaker, frozen so the §2-style double-investment
  bias cannot resolve the tie in favor of the layout change.

This direction is the same direction T2.7 §2 binds the SG2 author against
for the SG3 gate ("decided **solely** by the frozen numeric bar in §3
below, computed from the benchmark, with **no author override** of the
numeric outcome"). The subset inherits that binding.

### §3.6 Pilot rule (inherited from T2.7 §3.1 pass-2)

A tiny **unscored harness-validation pilot** is permitted (e.g. one run
per cell per substrate, ≤ 4 runs total) solely to confirm the harness
records subset metrics correctly on `maw@new-layout`. Such pilot data
MUST be excluded from the §3.1 verdict, MUST NOT be used to set or tune
any bar, and MUST NOT be cited in the T3.5 go/no-go writeup. Setting
bars from a pilot would turn it into a disguised measurement and is a §0
freeze-clause violation.

---

## §4 Substrates evaluated (recap; rivals out of scope)

| substrate id           | in subset? | reason                                                                                                       |
| ---------------------- | ---------- | ------------------------------------------------------------------------------------------------------------ |
| `maw@old-layout`       | YES        | the "before" arm — current `ws/` layout (v2 bare repo)                                                       |
| `maw@new-layout`       | YES        | the "after" arm — normal root checkout + hidden `.maw/worktrees/` (per `notes/sg3-layout-design.md`)         |
| `git-worktrees-bare`   | NO         | rival; gate is maw-vs-maw (T2.7 §3.1)                                                                        |
| `claude-native-worktrees` | NO      | rival; gate is maw-vs-maw (T2.7 §3.1)                                                                        |
| `jj-workspaces`        | NO         | rival; gate is maw-vs-maw (T2.7 §3.1)                                                                        |

Rivals continue to run in the SG2 master sweep (T2.6) on the eventually
shipped layout (whichever wins T3.5). The subset narrows the question to:
"does the new layout make agents worse at maw?" — nothing else.

---

## §5 Failure mode (EXPLICIT)

**If T3.5 observes regression beyond any §3.1 bar:**

1. **SG3 does NOT ship in v1.0.** SG3 is rolled to v1.1 (matching the
   T2.7 §3.1 / §11 framing of "must NOT block the trust artifact" and
   matching the SG3 parent bone `bn-2yh1` clause: "GATED on a
   pre-registered ergonomics eval showing no agent regression vs the
   current layout").
2. **v1.0 launches on the current `ws/` layout.** No layout-change
   branch is merged into the release. The release notes record the
   layout-eval as an explicit, acceptable, non-failure outcome (a
   NO-GO is a first-class result per T2.7 §2 / §3.1 / §11).
3. **The §3.1 bar's NO-GO data is published with the v1.0 release**,
   not buried. T2.7 §2's "publish the loss/overkill regime" commitment
   extends here: a NO-GO writeup is part of the v1.0 trust artifact,
   not a private internal note.
4. **The T3.5 writeup MUST state explicitly**, in the go/no-go section:
   "this NO-GO is an acceptable outcome per `bn-iux4` §5; v1.0 ships
   on `ws/`; SG3 follow-up is v1.1." This wording is pre-registered
   here so the writeup cannot drift into language that softens the
   defer outcome.

**If T3.5 observes no regression (subset GO):**

1. SG3 implementation branch is merged into v1.0.
2. The T3.5 writeup MUST publish the §3.4 Wilson-upper-bound CIs even
   though the verdict is GO — a GO subset is consistent with up to the
   per-bar MDE of true regression (§3.3). The writeup MUST cite this
   explicitly so a future SG4 / v1.1 review can revisit the layout
   decision if the SG2 master sweep (post-T3.5) surfaces a regression
   the subset was underpowered to see.

**If T3.5 results are AMBIGUOUS (e.g. SUB-A passes R2 but SUB-B fails
R2 by a hair):**

- §3.5's "ties go to the old layout" tiebreaker resolves the bar.
- A per-cell mixed result (one cell GO, one cell NO-GO) RESOLVES AS A
  NO-GO for the whole subset. This is the stricter resolution; a
  per-bar mixed result is by construction a NO-GO if the failing cell's
  bar is tripped per §3.1's "ALL of the following hold across BOTH
  SUB-A and SUB-B".

**Amendment trap.** If, after seeing T3.5 data, the author wishes to
move a §3.1 bar, that impulse is the bias firing (§3.5) and §0
forbids it. A §7 amendment is allowed only for pre-data methodological
fixes; a post-data amendment that flips a NO-GO to a GO is a
frozen-clause violation. The audit-trail record of this rule is the
explicit-failure-mode statement above.

---

## §6 Reproducibility manifest (the runnable subset)

### §6.1 Seeds (deterministic)

The subset uses `derive_seed(base_seed, cell, arm, rep)` from
`crates/maw-bench-sweep/src/grid.rs` (see `derive_seed` and `mix` / `fnv64`
primitives). For T3.5:

- `base_seed = 0x5G_3I_UX_4` (frozen 64-bit value:
  `0x5e_3e_4e_4e_5e_3e_4e_4e` — any frozen value works; T3.5 records the
  base_seed in its manifest and never changes it post-run).
- Per-(cell, arm, rep) seed = `derive_seed(base_seed, cell, arm, rep)`.
- Replicates: `rep ∈ [1, N]` where N = 20 at SUB-A and N = 10 at SUB-B.
- Pairing: `derive_seed` mixes `arm` last, so a paired comparison
  (`maw@old-layout` rep _k_ vs `maw@new-layout` rep _k_ at the same cell)
  uses the SAME upstream mix value for cell+t-class+base-seed and differs
  only by the arm-suffix mix. The bootstrap pairing per T2.7 §6.2 binds
  the two arms at the same `rep` index.

### §6.2 Sweep recipe (concrete commands T3.5 will run)

The subset is a `SweepGrid` projection:

```rust
use maw_bench_sweep::grid::{SweepCell, SweepGrid, ConditionPoint, TClass, frozen_spectrum};

let s = frozen_spectrum();
let c0 = s[0].clone();   // C0 benign
let c2 = s[2].clone();   // C2 moderate

let cells = vec![
    SweepCell { condition: c0, t_class: TClass::T0 },  // SUB-A
    SweepCell { condition: c2, t_class: TClass::T0 },  // SUB-B
];

// Two passes: N=20 at SUB-A, N=10 at SUB-B.
let sub_a_grid = SweepGrid {
    cells: vec![cells[0].clone()],
    arms: vec!["maw@old-layout".into(), "maw@new-layout".into()],
    seeds_per_cell: 20,
    base_seed: 0x5e3e_4e4e_5e3e_4e4e,
};
let sub_b_grid = SweepGrid {
    cells: vec![cells[1].clone()],
    arms: vec!["maw@old-layout".into(), "maw@new-layout".into()],
    seeds_per_cell: 10,
    base_seed: 0x5e3e_4e4e_5e3e_4e4e,
};
```

T3.5 ships a `sg3-subset` binary in `crates/maw-bench-sweep/src/bin/`
(T3.5-owned, not this bone) that constructs exactly the two grids
above, runs them via `SweepDriver`, and emits BenchRun JSONs to an
artifact directory. The §6.4 manifest (per T2.7) carries
`arm == "maw@old-layout"` or `arm == "maw@new-layout"` and the
T3.2-pinned layout-implementation commit SHA in `maw_version`.

### §6.3 Block-randomized run order (inherited from T2.7 §6.2)

For each `(condition, T-class, seed, replicate)` cell, the TWO subset
arms (`maw@old-layout`, `maw@new-layout`) run in a **randomized order
generated and committed BEFORE T3.5's first measured run**, seeded
from the subset's base_seed. **Neither arm may complete all replicates
for a cell before the other arm starts.** This blocks against temporal
drift in hosted-model behavior per T2.7 §7 R11. The randomized
schedule is part of T3.5's committed subset config and is published
with the subset artifacts. Any subset-attributed metric difference
that disappears when grouped by `arm_order_index` MUST be flagged in
the T3.5 writeup.

### §6.4 Oracles (inherited from T2.4)

- **Scenario oracle** (T2.4 / `BenchRun.oracle_b`): unchanged. The
  oracle is substrate-neutral by construction — it reads the
  `StateSnapshot` per-substrate ref layout. For `maw@new-layout` the
  StateSnapshot reads `.maw/worktrees/<name>` instead of `ws/<name>`;
  the adapter (T2.3) is responsible for this; the oracle's _rule_ is
  unchanged.
- **Auth health-gate** (T2.7 §8.2): unchanged; both arms checked
  per-run; auth-failure → `discard_auth` per §8.7.
- **Workspace-trust preflight** (T2.7 §8.6): N/A for the subset
  because both subset arms are maw, not `claude-native-worktrees`.
- **Wedge-incident flag** (T2.7 §1.1 / §7 R6): unchanged — divergent-
  state recovery OR abandoned committed work OR `turns_to_done > 1.5
  × arm-median-of-the-benign-condition`. For the subset, the "benign-
  condition" median is computed per ARM at SUB-A; the wedge flag at
  SUB-B compares against the same-arm SUB-A median (so the layout
  change cannot be flagged as a "wedge" merely for shifting the
  benign-condition median).

### §6.5 Version-capture manifest (inherited from T2.7 §6.4)

T3.5 emits, per run, the T2.7 §6.4 manifest verbatim, with the
following subset-specific clarifications:

- `arm ∈ {"maw@old-layout", "maw@new-layout"}` (subset arms).
- `maw_version` records BOTH the commit SHA of the maw binary AND the
  layout-implementation commit SHA (T3.2) for the `maw@new-layout`
  arm; the `maw@old-layout` arm records the v1.0-candidate maw SHA on
  the current `ws/` layout.
- `condition_id ∈ {"C0", "C2"}` (subset conditions).
- `t_class == "T0"` (subset T-class).
- `discard_class` / `discard_reason` per T2.7 §8.7 vocabulary
  unchanged.

### §6.6 Friction-list reproducibility

Per §1.4, T3.5 runs `just sg2-friction-list <subset-artifact-dir>`
TWICE (once per subset arm's artifacts) to produce
`friction-list-old.json` and `friction-list-new.json`. The diff is
included in T3.5's writeup as "subset friction old vs new" alongside
the gate verdict.

---

## §7 Amendment log

_(Empty at freeze. Any post-`2026-05-25T00:00:00Z` change to a frozen
value MUST be appended here per the §0 freeze clause: ISO-8601 UTC
timestamp, reason, authorizer, superseded value left readable, and
committed BEFORE the affected T3.5 run. No entry may retroactively
declare a missed bar met. Review-pass edits made BEFORE T3.5's first
measured run are NOT amendments — they are pre-acceptance revisions
audited here with full disposition.)_

— no amendments —

---

## §8 Acceptance-criteria checklist (spec `bn-iux4`)

- [x] **AC1**: `notes/sg3-subset-prereg.md` (this doc) is the new
      committed artifact under default. Frozen BEFORE any SG3
      implementation commit lands — see §0 pre-run precondition.
- [x] **AC2a**: Subset of SG2's pre-registered seeds named — §1.1
      (SUB-A = C0×T0 N=20/arm; SUB-B = C2×T0 N=10/arm) with §1.3 MDE
      / power rationale.
- [x] **AC2b**: Metrics carried forward verbatim from T2.4 — §2
      (correctness/safety axis §2.1; efficiency axis §2.2; friction
      axis §2.3 informational-only; per-verb attribution §2.4).
- [x] **AC2c**: Substrates evaluated — §1.2 and §4
      (`maw@old-layout` vs `maw@new-layout`; rivals NOT in subset).
- [x] **AC3a**: Per-metric regression rule — §3.1 (R1–R6).
- [x] **AC3b**: Dominance-axis rule — §3.2.
- [x] **AC3c**: MDE the chosen N actually has power for — §1.3 +
      §3.3 (computed from SP3 §3 / §4 + T2.7 §6.1).
- [x] **AC3d**: Pre-registered direction (superiority-or-equivalence,
      biased against the layout change, ties go to old layout) — §3.5.
- [x] **AC4**: Explicit FAILURE-MODE statement — §5 (SG3 rolls to
      v1.1; v1.0 launches on current `ws/` layout).
- [x] **AC5**: CI gate — `just sg3-prereg-check` recipe + CI step in
      `.github/workflows/dst.yml` (job `sg3-prereg-check`). The recipe
      asserts the doc exists AND its commit timestamp predates any
      modification to `crates/maw-cli/src/workspace/create.rs` (a
      `find`/`git log` date check; documented "frozen-before-the-fact"
      signal).

**All acceptance criteria met at freeze.**

---

## §9 Implications for downstream bones

- **T3.2 `bn-2sw3` (layout implementation):** the `crates/maw-cli/src/
  workspace/create.rs` file is THE canary the CI gate watches. Any
  commit modifying `create.rs` AFTER `notes/sg3-subset-prereg.md`'s
  commit timestamp is GREEN through the CI gate; any commit modifying
  `create.rs` BEFORE the doc's commit timestamp is RED. This means
  T3.2 cannot start modifying `create.rs` until this doc is in `main`.
- **T3.3 `bn-3kkl` (v2→new-layout migration):** the migration produces
  the `maw@new-layout` substrate adapter (T2.3 dependency). The
  adapter's substrate id MUST match this doc's §1.2 names exactly
  (`maw@old-layout` / `maw@new-layout`) so the §6.2 sweep recipe is
  literally runnable.
- **T3.4 `bn-1jqo` (guardrail relocation):** the AGENTS.md /
  workspace-path guardrail change must NOT regress any §3.1 bar.
  T3.4's acceptance is gated through T3.5's subset; this doc is the
  bar T3.4 also clears.
- **T3.5 `bn-1uzn` (the run + go/no-go):** this doc IS T3.5's input
  contract. T3.5 executes §6, evaluates §3 mechanically, and writes a
  go/no-go that cites this doc's §0 freeze SHA as the binding bar.
  T3.5 cannot soften the bar; T3.5 cannot add a substrate not in §1.2;
  T3.5 cannot change the MDE retroactively.
- **SP5 `bn-2kgu` (directional spike):** SP5 is the FAST directional
  read for the layout change; its findings inform T3.2 strategy but do
  NOT replace this doc as the FORMAL bar. SP5 may pass without
  unblocking T3.5; T3.5 requires this doc + the SG2 baseline.
- **T5.3 publication:** the publication MUST cite this doc's §0 freeze
  SHA when reporting the layout decision; the writeup MUST adhere to
  §3.4 Wilson-upper-bound discipline and §5's failure-mode language
  verbatim.
