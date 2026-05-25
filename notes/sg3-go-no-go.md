# SG3 Layout-Eval Go/No-Go (T3.5 / bn-1uzn)

**Bone:** `bn-1uzn` · parent SG3 `bn-2yh1` · task · size m
**Pre-registration anchor:** `notes/sg3-subset-prereg.md` (bn-iux4),
frozen `2026-05-25T00:00:00Z`. This document is the **load-bearing
mechanical verdict** the v1.0 release gate (SG5) reads to decide
whether the SG3 layout change merges into v1.0 or rolls to v1.1.
**Date scaffolded:** 2026-05-25.

## §0 Verdict slot (REAL-RUN)

> **VERDICT (real run):** _empty until the real-LLM campaign runs_
>
> **Real-run pre-reg pin:** `notes/sg3-subset-prereg.md` §0 freeze
> SHA `<TBD by real-run reporter>`.

The verdict above is filled by `sg3-layout-eval --decision-json
notes/sg3-go-no-go.real.json` after a real-LLM SUB-A + SUB-B campaign.
Until then, the §5 "REAL-RUN RESULT" template below is the
fill-in-the-blank scaffold the real-run reporter completes.

**Important:** the §1.4 pilot result below is HARNESS-VALIDATION
ONLY per bn-iux4 §3.6. Pilot numbers MUST NOT be quoted as the
binding verdict. The pilot exists to prove the eval harness is
wired correctly; the real-LLM campaign is the binding measurement.

---

## §1 Eval harness summary

### §1.1 What the harness does

`sg3-layout-eval` (in `crates/maw-bench-sweep/src/bin/sg3_layout_eval.rs`)
drives the bn-iux4 §6.2 frozen subset:

- **SUB-A**: `C0×T0`, N=20 paired runs (`maw@old-layout` /
  `maw@new-layout`).
- **SUB-B**: `C2×T0`, N=10 paired runs (same pairing).

Each run produces one `BenchRun` JSON written under
`<artifact-dir>/<arm>/<cell>/`. The arm name in each manifest is the
bn-iux4 §1.2 frozen value (`maw@old-layout` / `maw@new-layout`).
After both arms complete the harness aggregates each arm's
artifacts via `aggregate_artifacts(...)` and feeds the two summaries
through `decide_go_no_go(old, new, paired, bars)` from
`crates/maw-bench-sweep/src/sg3_decision.rs`.

### §1.2 Decision-rule contract

`decide_go_no_go` implements bn-iux4 §3.1 R1–R6 verbatim. Each rule
is evaluated **on both SUB-A and SUB-B** and the §3.1
all-of-the-following-on-both-cells gate is enforced. Per bn-iux4
§3.5 "ties go to the old layout" — a point estimate exactly at a
margin RESOLVES TO NO-GO via `RuleStatus::FailBorderline`.

| rule | metric                       | gate                                                                                                                          |
| ---- | ---------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| R1   | `irrecoverable_lost_work`    | new == 0 across all subset runs (any > 0 ⇒ NO-GO; hard bar)                                                                  |
| R2   | `workflow_loss` rate         | new ≤ old + 0.05 absolute                                                                                                     |
| R3   | `wedge_incident` rate        | new ≤ old + 0.10 absolute                                                                                                     |
| R4   | median `turns_to_done`       | NO-GO iff paired bootstrap 95% CI for `(new − old)` excludes 0 on the worse side AND median ratio `new/old` > ×1.15           |
| R5   | median `tool_calls_total`    | same as R4 (separate per-metric materiality)                                                                                  |
| R6   | `interventions` total        | new ≤ old (no net increase)                                                                                                   |

### §1.3 What this harness does NOT do

- It does NOT compute the paired bootstrap for R4 / R5. The
  bootstrap is the production-run analysis step that produces the
  `paired_ci_excludes_zero_on_worse_side` boolean; the eval harness
  consumes it. **Pilot mode** falls back to "ratio-only" R4 / R5
  (the pilot does not have N large enough for a meaningful
  bootstrap and bn-iux4 §3.6 forbids pilot numbers from setting
  bars anyway).
- It does NOT compute Wilson upper bounds on the verdict — those
  are a **reporting** discipline per bn-iux4 §3.4 applied in §5
  below. The aggregator's `work_lost_rate_ci.upper` is the carrier;
  the writeup template reads it directly.
- It does NOT mutate the bars. The bars are pre-registered (bn-iux4
  §3.1, frozen) and `decide_go_no_go(_, _, _, PrereggedBars)` takes
  them as a parameter so a §7 amendment can update them with a logged
  diff — never silently.

### §1.4 Pilot result (MockAgent + N=3, harness-validation only)

**Per bn-iux4 §3.6: pilot numbers DO NOT feed the §0 verdict.** The
pilot exists to confirm the harness wires correctly. Reproduce
with `just sg3-layout-eval-pilot`.

| arm                 | cell  | N | turns_to_done (median) | tool_calls_total (median) | work_lost_events |
| ------------------- | ----- | - | ---------------------: | ------------------------: | ---------------: |
| `maw@old-layout`    | C0×T0 | 3 |                      1 |                         0 |                0 |
| `maw@old-layout`    | C2×T0 | 3 |                      1 |                         0 |                0 |
| `maw@new-layout`    | C0×T0 | 3 |                      1 |                         0 |                0 |
| `maw@new-layout`    | C2×T0 | 3 |                      1 |                         0 |                0 |

(MockAgent finishes every run in one turn with zero tool calls; the
pilot's purpose is end-to-end wiring, not behavioral signal. Real
agents will produce variable per-replicate values.)

**Pilot decision:**

```
verdict       : GO
evidence rules: 12 (R1..R6 × SUB-A + SUB-B)
rules passed  : 12
```

**Pilot planted-regression sanity check** (writing one Oracle-B Red
into the new-arm SUB-A directory):

```
verdict           : NO-GO
regression_rule   : R1
regression_metric : irrecoverable_lost_work
by_amount         : "new-layout work_lost_events = 1, old = 0; §3.1 R1 = exactly 0"
```

Both pilot assertions confirm the harness wiring + decision logic
behave as the bn-iux4 §3.1 bar requires.

---

## §2 Calendar / cost for the real-LLM campaign

Per bn-iux4 §1.3:

- **60 measured runs** total (20 + 20 + 10 + 10 paired).
- ≈ **$0.08 happy-path per run**, ≈ **$0.14 wedge-path per run** at
  SP3 rates.
- **Subset campaign cost: ≈ $5–8** (well below the SG2 master sweep
  envelope; the subset is _additionally_ runnable, not a
  substitute).
- **Wall budget** (assuming serial Claude calls at ~30s/run plus
  retry headroom): **≈ 30–60 minutes** of model time, well inside a
  single calendar day.
- **Discard discipline**: per bn-iux4 §6.2 + T2.7 §8.7, any
  discard_class != measured row is logged in the run manifest;
  reaching 60 _measured_ runs may require up to ~75 attempts at
  current SP3 discard rates.

Scheduling: the real run can land as a single calendar block once
the §3 production prerequisites are met. Until then, this doc's §0
verdict slot is the bn-1uzn deliverable; the §5 fill-in is the
calendar artifact tracked separately.

---

## §3 Real-run prerequisites (precondition checklist)

Before running `just sg3-layout-eval` against real Claude:

- [ ] **T3.2 (bn-2sw3) layout implementation shipped.** The
      `maw@new-layout` substrate adapter exists with the bn-iux4
      §1.2 frozen arm id.
- [ ] **T3.3 (bn-3kkl) `maw migrate` shipped.** Migration converts
      a v2 `ws/` repo to the consolidated `.maw/workspaces/` layout
      so the substrate adapter has something to drive.
- [ ] **T2.2 real-LLM driver wired.** The substrate-factory closure
      passed to `SweepDriver::drive` must produce a real-Claude-
      bearing harness (the current pilot uses NoopSubstrate +
      MockAgent and is HARNESS-ONLY).
- [ ] **Auth + health-gate** per T2.7 §8.2 / §8.6 pass against the
      target Claude model id.
- [ ] **Pre-reg freeze SHA pinned**: the `notes/sg3-subset-prereg.md`
      §0 freeze commit SHA recorded as `prereg_freeze_sha` in the
      real-run manifest. `decide_go_no_go` does not enforce this
      pin; the writeup reporter MUST.
- [ ] **Harness commit pin recorded** per T2.7 §6.4: the
      `benchmark_harness_commit` in the manifest must be ≥ bn-iux4
      §0 harness pin `cd055004120cec4ceb7fb5e3f9b6d7d9e7899e1a`.
- [ ] **Block-randomized run order** per bn-iux4 §6.3: the
      randomized arm-interleaving schedule is committed BEFORE the
      first measured run (the current binary runs arms sequentially;
      the production wrapper must shuffle).
- [ ] **Bootstrap pipeline wired**: a downstream script reads the
      per-arm BenchRun JSONs, computes the paired bootstrap CI for
      `(new − old)` on `turns_to_done` and `tool_calls_total` per
      cell, and supplies the resulting `PairedCiSignals` map to
      `decide_go_no_go`. Without this, R4 / R5 run in "ratio-only"
      pilot mode and cannot trip — production runs MUST supply
      bootstrap signals.

---

## §4 Decision-rule trace (real-run JSON contract)

The real-run command:

```bash
just sg3-layout-eval \
  --layout=both \
  --n-a=20 --n-b=10 \
  --artifact-dir=runs/sg3-layout-eval-<YYYY-MM-DD> \
  --decision-json=notes/sg3-go-no-go.real.json
```

produces `notes/sg3-go-no-go.real.json` with this shape (this is the
serialized form of `maw_bench_sweep::Decision`):

```json
{
  "verdict": "go" | "no_go",
  "evidence": {
    "rules": [
      {
        "rule_id": "R1",
        "cell_id": "C0-T0",
        "metric": "irrecoverable_lost_work",
        "old_value": "0",
        "new_value": "0",
        "status": "pass" | "pass_improved" | "pass_equivalent" | "fail" | "fail_borderline" | "fail_missing_data",
        "rationale": "hard bar: new-layout work_lost_events = 0, old = 0; §3.1 R1 = exactly 0 across all subset runs"
      },
      "...12 rows: R1..R6 × SUB-A + SUB-B..."
    ]
  },
  "regression_rule":   "<R1..R6, only when verdict=no_go>",
  "regression_metric": "<metric name, only when verdict=no_go>",
  "by_amount":         "<short rationale, only when verdict=no_go>"
}
```

The §5 fill-in below reads from this JSON. The decision-logic
contract is the load-bearing artifact: SG5 reads it to gate
release. **The verdict is mechanical; the human reporter does not
override it.**

---

## §5 REAL-RUN RESULT (template — fill on real-LLM campaign day)

> **Status:** TEMPLATE — to be filled by the T3.5 real-run reporter
> using the JSON from §4. Until filled, the §0 verdict slot remains
> empty and SG5 reads "real run pending".

### §5.1 Real-run manifest

| field                          | value                                         |
| ------------------------------ | --------------------------------------------- |
| Run date (UTC)                 | `YYYY-MM-DD`                                  |
| Operator                       | `<name>`                                      |
| `prereg_freeze_sha`            | `<SHA of notes/sg3-subset-prereg.md at run>`  |
| `harness_commit`               | `<SHA of HEAD when sg3-layout-eval ran>`      |
| `claude_model_id`              | e.g. `claude-3-7-sonnet-20250219`             |
| `maw_old_layout_sha`           | `<maw binary SHA for old-layout arm>`         |
| `maw_new_layout_sha`           | `<maw binary SHA for new-layout arm>`         |
| `arm_order_schedule_sha`       | `<SHA of committed run-order schedule>`       |
| `discard_count`                | `<total discarded runs across both arms>`     |
| `measured_count`               | `60` (must match SUB-A 40 + SUB-B 20)         |

### §5.2 Per-cell aggregate (publication form per T2.7 §4.1)

```
CELL: SUB-A (C0×T0, N=20/arm)

                              maw@old-layout         maw@new-layout
  --- correctness/safety axis (higher-is-worse; 0 is the bar) ---
  irrecoverable_lost_work     <k>/<n> [Wilson 95%]   <k>/<n> [Wilson 95%]
  workflow_loss rate          <p>     [Wilson 95%]   <p>     [Wilson 95%]
  interventions (total)       <total>                <total>
  wedge_incident rate         <p>     [Wilson 95%]   <p>     [Wilson 95%]
  --- efficiency axis (lower-is-better; not safety) ---
  turns_to_done (med)         <m> (lo–hi)            <m> (lo–hi)
  tool_calls_total (med)      <m> (lo–hi)            <m> (lo–hi)

CELL: SUB-B (C2×T0, N=10/arm)
  ... same shape ...
```

Wilson 95% CIs MUST be published for every "0 observed" cell per
bn-iux4 §3.4 — never `rate = 0`. The aggregator's
`CellAggregate.work_lost_rate_ci` carries the formatted form.

### §5.3 Per-rule verdict (from §4 JSON, R1..R6 × SUB-A + SUB-B)

For each of the 12 rule rows, render one bullet:

- **`<rule_id>` @ `<cell_id>` (`<metric>`):** `<status>` —
  `<rationale>`.

### §5.4 Mechanical verdict

**Verdict:** `GO` | `NO-GO`

- **If GO**: the layout change merges into v1.0. SG3 is part of
  the v1.0 release.
- **If NO-GO** (per bn-iux4 §5 — an acceptable, non-failure
  outcome): **v1.0 ships on the current `ws/` layout. SG3 rolls to
  v1.1.** This wording is the pre-registered language and MUST be
  reproduced verbatim in the release notes; bn-iux4 §5 forbids
  softening this language.

### §5.5 Friction-axis report (bn-iux4 §1.4 / §6.6)

Per bn-iux4 §1.4 the friction signal is informational and does NOT
trip the gate, but it MUST be reported alongside. Run:

```bash
just sg2-friction-list runs/sg3-layout-eval-<YYYY-MM-DD>/maw-old-layout
just sg2-friction-list runs/sg3-layout-eval-<YYYY-MM-DD>/maw-new-layout
```

and paste the diff here. Flag any cluster appearing in
`maw@new-layout` top-3 that was NOT in `maw@old-layout` top-3 as a
"new-layout-introduced friction" finding (informational, not gate).

### §5.6 Dominance-axis directional signal (bn-iux4 §3.2)

If R4 / R5 pass at both cells but the (new − old) median
differences are adverse at BOTH cells, surface here as a
"directional adverse signal below materiality threshold". This is
informational per pre-committed materiality.

### §5.7 Honest disclosure (bn-iux4 §3.3 / §6.1)

Bars vs MDE are NOT the same. Even on a GO, publish:

> "GO on the bn-iux4 §3.1 bars; consistent with up to ~+0.20
> absolute regression at SUB-A and ~+0.30 at SUB-B on the
> rate metrics (the Wilson upper bound at the subset N); see
> bn-iux4 §1.3 detectable-effects table."

---

## §6 NO-GO branch (pre-registered language; bn-iux4 §5)

If the §5.4 verdict is **NO-GO**, this section is the binding
release-notes / publication language. Do not paraphrase — copy
verbatim.

> _v1.0 ships on the current `ws/` layout. The SG3 layout-eval
> (`bn-1uzn`) returned NO-GO against the bn-iux4 §3.1 bars; per
> bn-iux4 §5 this is an acceptable, pre-registered outcome. SG3
> rolls to v1.1 as the immediate follow-up. The §3.1 bar's NO-GO
> data is published with the v1.0 release (not buried) per T2.7
> §2 commitment._

This is the wording bn-iux4 §5 step 4 mandates. It MUST appear in:

1. The v1.0 release notes (the trust artifact).
2. The publication (T5.3) layout-decision section.
3. The SG5 release-gate handoff that reads this doc.

---

## §7 GO branch (release-notes language; for completeness)

If the §5.4 verdict is **GO**, the release-notes language is:

> _v1.0 ships on the consolidated `.maw/workspaces/` layout
> (SG3, `bn-2yh1`). The SG3 layout-eval (`bn-1uzn`) returned GO
> against the bn-iux4 §3.1 bars on the SUB-A + SUB-B subset; per
> bn-iux4 §3.4 Wilson-upper-bound discipline the GO is
> consistent with up to ~+0.20 absolute regression at SUB-A and
> ~+0.30 at SUB-B on the rate metrics (the detectable-effect
> ceiling at the subset N). The SG2 master sweep (T2.6,
> post-T3.5) will revisit any regression the subset was
> underpowered to see._

This wording is bn-iux4 §5 step 1 + §3.4 discipline.

---

## §8 Constraints for SG5 (downstream — release gate)

**SG5 (the v1.0 release) reads this doc.** Specifically:

- SG5 MUST NOT release until the §0 verdict slot is filled.
- If §0 is `GO`, SG5 ships the consolidated layout in v1.0.
- If §0 is `NO-GO`, SG5 ships the `ws/` layout in v1.0 and rolls
  SG3 to v1.1. The §6 release-notes language is mandatory.
- SG5 MUST cite the `prereg_freeze_sha` from §5.1 and link this
  doc as the binding artifact (T2.7 §11 publication wiring).
- A `NO-GO` is not a failure for SG5: v1.0 still ships, on SG1
  alone. The whole point of bn-iux4's pre-registration is that
  the layout outcome does not block the trust artifact.

**Pre-real-run gate (HARD):** SG5 cannot read this doc as "GO"
until §3 prerequisites are checked AND `notes/sg3-go-no-go.real.json`
exists (the `--decision-json` output of the real run). Pilot output
is NOT a substitute (bn-iux4 §3.6).

---

## §9 Reproduction (pilot + real run)

```bash
# Pilot (≤ 60s, MockAgent + N=3, $0):
just sg3-layout-eval-pilot

# Real run (≈ 30-60 min wall, real Claude, ≈ $5-8):
just sg3-layout-eval --layout=both --n-a=20 --n-b=10 \
  --artifact-dir=runs/sg3-layout-eval-<YYYY-MM-DD> \
  --decision-json=notes/sg3-go-no-go.real.json

# Single-arm rerun (for retry / discard recovery; not the verdict):
just sg3-layout-eval --layout=new --n-a=20 --n-b=10 \
  --artifact-dir=runs/sg3-layout-eval-<YYYY-MM-DD>

# Pre-reg gate (the bn-iux4 CI check that this doc + the eval
# harness commit AFTER notes/sg3-subset-prereg.md):
just sg3-prereg-check
```

---

## §10 Amendment log

_(No amendments. Any post-real-run change to this document is
either a §5 fill-in or a §7 amendment to bn-iux4 §3.1; the latter
is forbidden after seeing data per bn-iux4 §0 freeze clause.)_

— no amendments —
