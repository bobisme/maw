# SG3 NO-GO Root Cause v2 (bn-1pzb)

Sequel to `notes/sg3-no-go-rootcause.md` (bn-2ert / H5 version skew).
Investigation of the 2026-05-27 SG3 RE-RUN that REPRODUCED the R6 C2
interventions NO-GO against the post-T3.2 binary (data committed at
`notes/eval-real-2026-05-27/sg3-rerun/`).

---

## §0 Summary (1-sentence)

**Root cause**: the R6 NO-GO is a **measurement artifact (H8)** —
unpaired per-arm seeds randomly gave the new-layout cell **one more
"Recover the previously destroyed workspace" task** than the old-layout
cell (5 vs 4 of 10), which flipped the lower-median fire count from 0
to 1 and the `median × n` proxy amplified that 1-vs-0 median delta into
the reported `total(new) = 10, total(old) = 0`; the metric is
**counting the agent correctly executing the task**, not friction.

**Recommended fix**: a 3-part metric cleanup
([§5 Fix recommendation](#5-fix-recommendation)) — (a) make `is_maw_arm`
in `extract.rs:116` recognise `maw@<flavor>` so both arms route through
the principled T2.5 attribution path that requires `prior_outcome.conflicted`;
(b) pair seeds across arms in `sg3_layout_eval` so the same task
batteries appear on both sides (variance-reduction); (c) replace the
`median × n` proxy in `sg3_decision::sum_proxy` with the per-replicate
raw sum that is already available from `MetricRecord`. **None block
v1.0-pre.1** in code terms — they are correctness fixes for the SG3
**measurement**, not the maw substrate.

**In-scope for v1.0-pre.1**: ship the consolidated `.maw/` default
unchanged. The layout itself is fine; the NO-GO is in the bench harness.
The §6 recommendation revises bn-2ert's option (c): keep the layout AND
do **not** burn another eval-run cycle until the metric fix lands; once
fixed, expect R6 to PASS_EQUIVALENT without re-running anything.

---

## §1 H6 — Adapter-substrate mismatch check

**Verdict: PARTIALLY SUPPORTED as methodological debt, but NOT the
cause of R6.**

### What the adapter creates vs what `maw init --consolidated` creates

Built both, side-by-side, with the currently-installed post-T3.2
binary (Cargo.toml still pinned to `0.61.0`; binary built today
2026-05-27 14:02 from `main` HEAD `b5c6f212`).

#### Adapter (`ConsolidatedLayoutAdapter::new_in`, hand-rolled):

```
<root>/.git/                           ← REAL .git directory
<root>/.gitignore                      ← custom rules (3 lines)
<root>/README.md
<root>/.maw/.gitignore                 ← custom rules (5 lines)
<root>/.maw/config.toml                ← SP5-simulation comment + [layout] section
<root>/.maw/manifold/PLACEHOLDER       ← single placeholder file
<root>/.maw/workspaces/
<root>/.maw/cache/
```

#### Real `maw init` (consolidated):

```
<root>/.git                            ← 17-byte FILE: "gitdir: repo.git\n"
<root>/repo.git/                       ← actual git dir (relocated)
<root>/.gitignore                      ← /.maw/, /.manifold/, /repo.git/ (4 lines + header)
<root>/.maw/.gitignore                 ← * / !.gitignore / !config.toml (canonical form)
<root>/.maw/config.toml                ← [repo] branch = "main" (canonical)
<root>/.maw/manifold/epochs/
<root>/.maw/manifold/artifacts/{ws,merge}/
<root>/.maw/manifold/config.toml       ← [repo] branch = "main"
<root>/.maw/workspaces/
<root>/.maw/cache/
+ refs/manifold/epoch/current set to epoch₀
+ refs/manifold/workspaces/default/state set to epoch₀
```

### Divergences

1. **`.git` topology**: adapter keeps `<root>/.git/` as a real
   directory; real `maw init` normalises to `<root>/.git` (file) +
   `<root>/repo.git/` (relocated common-dir). The adapter substrate is
   missing the `repo.git/` indirection that the post-T3.2 binary
   produces.
2. **`.maw/manifold/` content**: adapter has a single `PLACEHOLDER`
   file; real init populates `epochs/`, `artifacts/{ws,merge}/`, and
   `config.toml`. The adapter does NOT initialise the manifold
   metadata layer the binary expects.
3. **Manifold refs**: adapter never sets `refs/manifold/epoch/current`
   or `refs/manifold/workspaces/default/state`. `maw doctor` against
   the adapter substrate reports `[OK] epoch drift: epoch ref not set
   (run \`maw init\`)` — it passes because that specific check is
   tolerant of greenfield state, but a real maw repo would have it.
4. **`.gitignore` contents differ** (both arms gitignore `.maw/` but
   adapter omits `/.manifold/` and `/repo.git/` rules; canonical text
   differs).
5. **`.maw/.gitignore` rules differ** — adapter uses explicit per-dir
   rules; canonical uses `*` + bang-allow pattern. Semantically
   equivalent for the consolidated-layout invariant but textually
   distinct.
6. **`.maw/config.toml` content differs** — adapter has the SP5
   simulation comment + `[layout] workspaces_dir = ".maw/workspaces"`;
   canonical has `[repo] branch = "main"`.

### Why this does NOT explain R6

`maw doctor` PASSES on the adapter's hand-rolled substrate (I ran it
in `/tmp/maw-adapter-test/` to verify): the post-T3.2 detect
heuristic (`<root>/.maw/manifold/` exists ⇒ ConsolidatedMawDir) sees
the `PLACEHOLDER`-only manifold dir and accepts it. Across all 10
new-layout C2 runs, `[FAIL]` count is 0 and `maw init` is never
invoked (vs 6/10 runs invoking init in the pre-T3.2 data). The
substrate divergence is real but is not load-bearing for the agent's
behaviour — the doctor advice is clean, no migration dance, and the
binary's core verbs (`maw ws create`, `maw ws destroy`, `maw ws
recover`) work end-to-end against the simulated substrate.

The asymmetric R6 fires are NOT caused by the substrate shape — see
§2 and §3.

**H6 verdict**: partially supported as **bench-harness methodological
debt** (the eval would be more honest if the new-layout adapter
called `maw init --consolidated` directly, matching what
`MawAdapter::new()` already does for the old-layout arm by calling
`maw init`). This should become a separate `s`-sized bone post-pre.1.
**Not the cause of the R6 NO-GO; not a v1.0-pre.1 blocker.**

---

## §2 H7 — Real C2-induced regression mechanism trace

**Verdict: REFUTED.**

Forensic per-run trace of all 10 new-layout C2-T0 BenchRuns plus all
10 old-layout C2-T0 BenchRuns. For each run I extracted:

- the **task battery** (4 abstract tasks per run from the prompt's
  `## Task battery` section);
- **whether that battery contains a "Recover the previously destroyed
  workspace" task** (the only recovery-vocabulary task in the
  scenario generator);
- **every turn that the substring heuristic
  `count_work_redone_turns` (extract.rs:246) attributes as
  "work-redone"**, with the exact tool-call args_json that fired.

### Per-run table (new-layout C2-T0)

Tasks abbreviated: `cr` = create, `ed` = edit, `cm` = commit,
`ds` = destroy, `df` = destroy --force, `RC` = recover, `mg` = merge.

| run | turns | tasks                | RC? | fires | fired call args (truncated)                                          |
|-----|------:|----------------------|:---:|------:|----------------------------------------------------------------------|
| r001|    10 | cr, ed, cr, cm       |  N  |     0 | —                                                                    |
| r002|    15 | cr, ds, RC, ds       |  Y  |     1 | `maw ws recover` (List recovery snapshots, turn 5)                   |
| r003|    16 | cr, cm, RC, ed       |  Y  |     1 | `maw ws recover` (List recovery snapshots, turn 5)                   |
| r004|    15 | cr, ds, RC, ds       |  Y  |     1 | `maw ws recover` (List recovery snapshots, turn 5)                   |
| r005|     7 | cr, cr, ed, cm       |  N  |     0 | —                                                                    |
| r006|    11 | cr, ed, ed, ed       |  N  |     0 | —                                                                    |
| r007|     8 | cr, ed, cm, df       |  N  |     1 | `maw ws destroy ws-0 --force` (matches `recover` substring in desc, turn 7) |
| r008|     9 | cr, cr, ed, cm       |  N  |     0 | —                                                                    |
| r009|    10 | cr, ed, cm, RC       |  Y  |     1 | `maw ws recover ws-0 --to ws-1` (turn 8)                             |
| r010|    15 | cr, ds, RC, ds       |  Y  |     2 | `maw ws recover` (turn 5) + `maw ws create --from main ws-1` (turn 12, "(recovery of ws-0...)" in desc) |

### Per-run table (old-layout C2-T0)

| run | turns | tasks                | RC? | fires | fired call args (truncated)                                          |
|-----|------:|----------------------|:---:|------:|----------------------------------------------------------------------|
| r001|     7 | cr, ed, cm, RC       |  Y  |     1 | `maw ws recover ws-0 --to ws-1` (turn 6)                             |
| r002|     9 | cr, cr, ed, ed       |  N  |     0 | —                                                                    |
| r003|    12 | cr, ds, RC, ds       |  Y  |     2 | `maw ws recover` (turn 5) + `maw ws recover --ref ...` (turn 10)     |
| r004|    10 | cr, ed, cr, cm       |  N  |     0 | —                                                                    |
| r005|    12 | cr, ed, cm, mg       |  N  |     1 | `maw ws merge ws-0 --into default --check` (matches `conflict` substring in desc, turn 8) |
| r006|     9 | cr, cr, cm, ed       |  N  |     0 | —                                                                    |
| r007|    14 | cr, ds, RC, ed       |  Y  |     1 | `maw ws recover` (turn 5)                                            |
| r008|    13 | cr, ds, RC, ds       |  Y  |     1 | `maw ws recover` (turn 5)                                            |
| r009|     4 | cr, cr, cm, cm       |  N  |     0 | —                                                                    |
| r010|    10 | cr, ed, ed, cm       |  N  |     0 | —                                                                    |

### Cross-tabulation: fires vs RC-task

|                            | new | old |
|----------------------------|----:|----:|
| Runs with RC task          |   5 |   4 |
| Of those, fires > 0        | 5/5 | 4/4 |
| Runs WITHOUT RC task       |   5 |   6 |
| Of those, fires > 0        | 1/5 | 1/6 |

The asymmetry is **exactly ONE extra run with an RC task in the
new-layout arm**. Every single run that has an RC task fires
(because the agent correctly runs `maw ws recover`, which trips the
substring heuristic by definition); zero runs without an RC task
fire on either arm except a single near-miss per arm where the
agent's description string happened to contain a recovery word
(`r007/new` description "recovery snapshot is captured automatically";
`r005/old` description "Dry-run merge to check for **conflicts**").

### Why the agents see the SAME conditions

The C2 cell is `{k_overlap_numerator=4, k_concurrency=3,
k_rounds=5}` for both arms. The scenario plan is generated by
`generate_plan(seed, &cell.condition.to_profile(), plan_steps)` and
the C2 profile is identical between arms. **What differs is the
seed**, because `derive_seed(base_seed, cell, arm, rep)` in
`grid.rs:259` mixes `arm` into the seed. So `maw@old-layout` and
`maw@new-layout` produce different scenario plans at the same
(cell, rep) — unpaired random draws of the task vocabulary.

Sampling N=10 from a population where ~50% of plans get an RC task
gives Binomial(10, 0.5) which has std ≈ 1.58. Observing 5 RC on one
arm and 4 on the other is well within noise: P(|new - old| ≥ 1) ≈ 0.75
under the null.

### Layout-shape mechanism check (the actual H7 question)

For each fire above, the action the agent took (`maw ws recover`,
`maw ws destroy ws-0 --force`, etc.) is **layout-agnostic** — it
operates on workspace names, not paths. The agent isn't navigating
`.maw/workspaces/ws-0/` vs `ws/ws-0/`; it's invoking maw verbs that
accept workspace names. So even if you believed the substring
heuristic counts a real friction event, there is no plausible
mechanism by which the .maw/ vs ws/ path shape would cause the
agent to issue `maw ws recover` more often. The CORRECT signal here
would be "agent fumbles a path that the layout shape made
ambiguous"; nothing of the kind is visible in the transcripts.

**H7 verdict: REFUTED**. There is no layout-shape mechanism causing
the R6 asymmetry. The fires are uniformly the agent doing exactly
what the task asked.

---

## §3 H8 — Attribution heuristic artifact check

**Verdict: STRONGLY SUPPORTED. This is the root cause.**

### The active code path

`MetricRecord` for both arms is built by `extract_metrics` in
`crates/maw-bench-metrics/src/extract.rs:60`. The branch at line 83:

```rust
let work_redone = if is_maw_arm(&run.manifest.arm) {
    per_verb_wasted_turns.values().map(|n| u64::from(*n)).sum()
} else {
    count_work_redone_turns(&run.transcript.turns)  // ← substring heuristic
};
```

with `is_maw_arm` at line 116:

```rust
fn is_maw_arm(arm: &str) -> bool {
    arm == "maw" || arm.starts_with("maw-")
}
```

The eval's arm names are `maw@old-layout` and `maw@new-layout`. **Neither
matches**: they neither equal `"maw"` nor start with `"maw-"` (the
delimiter is `@`, not `-`). So BOTH arms fall into the
`count_work_redone_turns` substring-fallback path.

(Confirmed — bn-2ert's writeup at §3 noted the same; the binary
shipped at v0.61.0 carries this bug and v0.61.0's metric path is
what the rerun harness still runs because the bench-side
`is_maw_arm` predicate has not been updated since.)

### What the substring heuristic counts

`count_work_redone_turns` at line 246 counts a turn iff EITHER:

1. **Recovery entry**: any tool call in the turn's `args_json`
   (case-insensitive) contains any of `conflict`, `ws conflicts`,
   `resolve`, `recover`, `rebase` — AND the prior turn does not.
2. **Literal retry**: any Bash call's `args_json` is byte-identical
   to a Bash call from the prior turn.

The substring set treats the literal `maw ws recover` invocation as
"work-redone". That is exactly the bone's H8 prediction: the proxy
is misclassifying **task-required, correct, expected** verb usage as
friction.

### What the principled T2.5 path WOULD have counted

The attribution path (`attribute_tool_call` in
`crates/maw-bench-metrics/src/attribution.rs:325`) DOES route on
recovery verbs (`WsRecoverInvoked` at line 89), but it gates them
through the `per_verb_attribution` walker which threads
`prior_outcome` — and the friction definition for non-intrinsic
verbs requires `prior_outcome.conflicted` or `!prior_outcome.ok`.
**However, `WsRecoverInvoked` is itself classified as "intrinsic
recovery"** (line 354):

```rust
if hay.contains("maw ws recover") {
    return Some(MawVerbAttribution::WsRecoverInvoked);
}
```

So even the principled path would attribute every `maw ws recover`
invocation as a wasted turn — including task-required ones. The
attribution module's docs warn: "Recover is intrinsically a
recovery op; no prior context needed" (line 596). That framing is
correct WHEN the recover invocation is responding to an unsolicited
loss — but it is wrong when the task battery literally instructs
the agent to call recover.

So even if the `is_maw_arm` predicate were fixed, R6 would still
trip on RC tasks. **Both heuristics are blind to task-intent**.

### The `median × n` amplification

`sg3_decision::sum_proxy` (sg3_decision.rs:721) computes
`median(work_redone_turns) × n` per cell. Computed on the rerun:

- new-layout fires per run (input order): `[0, 1, 1, 1, 0, 0, 1, 0, 1, 2]`
- sorted: `[0, 0, 0, 0, 1, 1, 1, 1, 1, 2]`, lower-median (index `(10-1)/2 = 4`) = **1**
- proxy: `1 × 10 = 10` — matches `decision.json: total(new) = 10`.

- old-layout fires per run: `[1, 0, 2, 0, 1, 0, 1, 1, 0, 0]`
- sorted: `[0, 0, 0, 0, 0, 1, 1, 1, 1, 2]`, lower-median = **0**
- proxy: `0 × 10 = 0` — matches `decision.json: total(old) = 0`.

The raw per-replicate **sum** is 7 (new) vs 6 (old) — a one-fire
difference, essentially noise. The `median × n` proxy turns it into
a 10× gap because integer-truncated lower-median is a 1-bit
quantization of the underlying distribution.

The pre-reg doc-comment on `sum_proxy` flags this:

> "Production callers feeding R6 should plumb the raw per-
> replicate sum once T2.5/T2.6 add the attribution-driven total."

That fix has not landed.

### Conclusion

H8 is **decisively supported**. Three layered defects collude:

1. `is_maw_arm` predicate mismatches `maw@<flavor>` arm names, so
   the bench falls back to the substring heuristic.
2. The substring heuristic counts the literal task-required
   `maw ws recover` invocation as "work redone".
3. `median × n` amplifies a 1-vs-0 lower-median into a 10-vs-0
   total proxy.

Unpaired per-arm seeds give different task-battery distributions,
so one arm randomly draws one more RC task than the other. That
single extra RC fire is all it takes to flip the lower-median, and
the proxy fans it to 10×.

---

## §4 Verdict

| Hyp | Description                                                       | Verdict     |
|-----|-------------------------------------------------------------------|-------------|
| H5  | Substrate/binary version skew (bn-2ert)                           | **RESOLVED** (post-T3.2 doctor is clean; 0 init invocations in rerun) |
| H6  | ConsolidatedLayoutAdapter mismatch vs `maw init --consolidated`   | **PARTIAL** (real divergences exist but do not cause R6) |
| H7  | Real C2 × .maw/ layout regression triggering agent retries        | **REFUTED**  (no layout-shape mechanism; agents do not navigate paths) |
| H8  | Attribution heuristic + median×n proxy mis-classifying task-required `maw ws recover` as friction | **SUPPORTED** (root cause; 3-layer defect documented in §3) |

Single-sentence root cause: **unpaired per-arm seeds + substring
heuristic counting task-required recovery as friction + median×n
proxy = a 10:0 NO-GO from a 7:6 raw-fire difference that is itself
just sampling variance in task-battery allocation**.

---

## §5 Fix recommendation

### Fix A (primary; bone size `s`): three-line metric-correctness patch

These are independent but composable; the smallest single change
that unbreaks R6 is **Fix A.2 alone**:

#### A.1 — `is_maw_arm` recognises `maw@<flavor>`

`crates/maw-bench-metrics/src/extract.rs:116`:

```rust
fn is_maw_arm(arm: &str) -> bool {
    arm == "maw" || arm.starts_with("maw-") || arm.starts_with("maw@")
}
```

This routes both `maw@old-layout` and `maw@new-layout` through the
T2.5 attribution path. **Necessary precondition for A.2 to take
effect.**

#### A.2 — Task-aware recovery attribution

`crates/maw-bench-metrics/src/attribution.rs:354` currently treats
every `maw ws recover` as `WsRecoverInvoked`. This is wrong when
the task literally instructs `recover`. Two implementation options:

**Option α (smaller)**: keep `WsRecoverInvoked` intrinsic but
**subtract task-required invocations from R6** in `extract.rs`. Read
the task-battery vocabulary off `BenchRun.transcript.prompt`,
detect the literal "Recover the previously destroyed workspace"
task, and decrement the cluster count by 1 per RC-task.

**Option β (better)**: thread a "task expectation" hint through
`ToolCall.attributed_op` at harness time. The scenario generator
knows which tasks it emitted; when an RC task is present, emit an
`OpClass::Recover` with a `task_intended: true` marker, and the
attribution branch skips friction-attribution for intended-recover
calls.

β is the right answer long-term; α is the pre.1-shippable patch.

#### A.3 — Replace `median × n` proxy with raw per-replicate sum

`crates/maw-bench-sweep/src/sg3_decision.rs:721`. The
`CellAggregate` struct stores only median+min+max; either add a
`sum` field to the aggregator (one-liner in
`aggregate_metric_records`) or change the rule callers to feed the
raw `Vec<MetricRecord>` directly.

The pre-reg doc-comment already says this is owed; just do it.

### Fix B (variance reduction; bone size `s`): pair seeds across arms

`crates/maw-bench-sweep/src/grid.rs:259` mixes `arm` into the seed.
For comparison cells where the goal is "same scenario, different
substrate", DROP the arm-mixing so paired arms see identical
scenarios. This eliminates the
"one-arm-randomly-got-an-extra-RC-task" failure mode for the entire
SG3 axis.

Either parameterize `derive_seed` with `paired: bool`, or add a
sibling `derive_paired_seed(base_seed, cell, rep)` and use it from
`sg3_layout_eval`. Risk: existing rendered tables (sg2-N=10) used
arm-mixed seeds; pairing changes the seeds for new runs only —
no schema break, no historical-data invalidation, but a small
in-table footnote that the seed convention rev'd.

### Fix C (data-quality cleanup; bone size `m`): substrate parity

`ConsolidatedLayoutAdapter::new_in` should call `maw init
--consolidated` directly, the same way `MawAdapter::new_in` calls
`maw init`. That makes the new-layout substrate byte-identical to
what real users will see, and removes the H6 methodological debt.

This is **NOT** a v1.0-pre.1 blocker. Once Fix A lands, the SG3
eval will pass on the current data without any re-run. Fix C
becomes appropriate when the bench harness next gets re-run for
v1.0-final (or v1.1) — it's the correct "honest measurement" hygiene
but it does not change the R6 conclusion.

### What we do NOT need to do

- **Re-run the SG3 eval to "see if Fix A works"** — the existing
  rerun data is sufficient. Compute the corrected R6 from the
  committed BenchRun JSONs offline; the §3 hand-computation already
  shows raw sum 7 vs 6 (Δ = +1, well within R6's `no net increase`
  bar's noise floor).
- **Revert T3.2 / change the v1.0 layout default** — the substrate
  is fine; the metric is broken.

---

## §6 v1.0-pre.1 recommendation for bn-3uj4

**Proposed (lead decides)**: ship `.maw/` consolidated layout as the
v1.0 default. Land Fix A.1 + A.2(α) + A.3 in the same `s`-sized bone
**before** cutting pre.1; do not require a fresh SG3 eval-run.

### Why this differs from bn-2ert's recommendation

bn-2ert's §6 recommended option (c): keep the layout default, install
the post-T3.2 binary, **re-run SG3 against the fixed binary**. The
re-run happened. The metric is still broken. Re-running again without
fixing the metric will reproduce the same NO-GO each time, with the
particular failing cell depending on which arm gets the RC-heavier
seed draw. We have to fix the meter, not the meter's input.

### Revised pre.1 acceptance footnote

Recommended footnote in the pre.1 release notes (replacing whatever
text was tentatively planned to absorb the 2026-05-26 NO-GO):

> **SG3 layout-eval status (2026-05-27 rerun)**: R6
> (interventions) showed `total(new) = 10, total(old) = 0` at C2.
> Forensic analysis (`notes/sg3-no-go-rootcause-v2.md`) attributes
> the asymmetry to the bench harness's `count_work_redone_turns`
> substring heuristic counting task-required `maw ws recover`
> invocations as friction, combined with unpaired per-arm seeds and
> the `median × n` proxy that amplifies one-bit median shifts to
> N×. Raw per-replicate fire sums are 7 (new) vs 6 (old) — within
> sampling noise. The metric will be fixed in a follow-up bone
> (`bn-???` opened post-pre.1); R6 PASS_EQUIVALENT expected on the
> existing rerun data once the fix lands.

### Cost

- Fix A.1 + A.2(α) + A.3: ~2-3 hours of code, ~1 hour of bench
  re-aggregation (no new LLM runs), 0 dollars.
- Fix B (paired seeds): another ~2 hours, 0 dollars, optional for
  pre.1 but recommended for v1.0-final.
- Fix C (adapter parity): ~half a day, in scope for v1.1.

Total v1.0-pre.1 critical path: one `s`-sized bone.

---

## §7 Open items for the implementor

- **The `WsRecoverInvoked` "always-intrinsic" framing in
  `attribution.rs:596`** ("Recover is intrinsically a recovery op;
  no prior context needed") should be revisited even outside Fix
  A.2. The unit tests at lines 596-602 codify the wrong invariant
  for the SG3 use case. Adding an `intended: bool` parameter to
  `attribute_tool_call` (default `false`) plus matching extractor
  context-threading is the principled long-term shape.
- **bn-2ert's Fix B was the same A.1 above** and was filed as
  "secondary cleanup, NOT v1.0-pre.1 blocker". It IS the blocker —
  upgrade urgency.
- **`bn-f5zu`** (manifest version-skew catcher, already landed) is
  the v0.61.0/post-T3.2 lookout. It works: every BenchRun in the
  rerun now carries `maw_version` in its manifest. The version
  string is still `"maw 0.61.0"` because Cargo.toml hasn't been
  bumped past 0.61.0 yet — that is a release-versioning gap (bump
  Cargo.toml to 0.62.0-pre.1 or similar at pre.1 cut so the
  manifest distinguishes the rebuilt binary from the released one).
- The R6 cell-aggregate JSON should carry both the median proxy AND
  the raw sum once Fix A.3 lands, so the contrast is visible in
  every decision document and the proxy's noise floor is no longer
  invisible.

---

## §8 Reproducibility appendix

All conclusions in this document are derived from data committed
under `notes/eval-real-2026-05-27/sg3-rerun/`. To re-derive the
per-run fire counts:

```python
import json, re, glob, os
def is_recovery(j):
    h = j.lower()
    return any(s in h for s in ["conflict", "ws conflicts", "resolve", "recover", "rebase"])
def fires(path):
    with open(path) as f: r = json.load(f)
    turns = r["transcript"]["turns"]
    n = 0
    for i, t in enumerate(turns):
        prev = turns[i-1] if i > 0 else None
        cur = [c for c in t.get("tool_calls", []) if is_recovery(c.get("args_json",""))]
        prev_r = prev and any(is_recovery(c.get("args_json","")) for c in prev.get("tool_calls", []))
        if cur and (prev is None or not prev_r): n += len(cur)
        if prev:
            for c in t.get("tool_calls", []):
                if c.get("name") == "Bash" and any(p.get("name") == "Bash" and p.get("args_json") == c.get("args_json") for p in prev.get("tool_calls", [])):
                    n += 1; break
    return n
base = "notes/eval-real-2026-05-27/sg3-rerun"
for layout in ["maw-new-layout", "maw-old-layout"]:
    counts = [fires(p) for p in sorted(glob.glob(f"{base}/{layout}/C2-T0/*.json"))]
    s = sorted(counts); med = s[(len(s)-1)//2]
    print(f"{layout}: fires={counts} sorted={s} lower_median={med} median*n={med*len(s)} sum={sum(counts)}")
```

Expected output:

```
maw-new-layout: fires=[0,1,1,1,0,0,1,0,1,2] sorted=[0,0,0,0,1,1,1,1,1,2] lower_median=1 median*n=10 sum=7
maw-old-layout: fires=[1,0,2,0,1,0,1,1,0,0] sorted=[0,0,0,0,0,1,1,1,1,2] lower_median=0 median*n=0 sum=6
```
