# SG2 metric definitions (T2.4 / bn-oko4)

**Status:** stable. One section per metric. The implementation
(`crates/maw-bench-metrics`) reads this doc as the rule of record;
any divergence is a bug in the implementation, not in the doc.

**Binding rule (from the bone + pre-reg §1.2 + §4):** these metrics
are reported as **separate axes** and are **never composited into a
single score, weighted sum, or ranking**. Each section labels the
metric's axis explicitly. Renderers — including
`sg2-report` and any future T2.6 / T5.x reporter — must preserve
this separation.

**Schema reference.** All "source field" entries below cite the
on-disk schema in `crates/maw-bench/src/run.rs` (`BenchRun`, schema
v1). The substrate-op vocabulary referenced as "canonical effect"
lives in `crates/maw-bench-adapters/src/lib.rs` (`StepOutcome`,
`StateSnapshot`). The two `Substrate` traits on `main` are
complementary, not duplicative — see the doc-comment on
`maw_bench_metrics`'s `lib.rs`.

**Pre-reg alignment.** Names match the frozen pre-reg
(`notes/sg2-benchmark-preregistration.md` §1.1) verbatim where they
exist. Two T2.4-introduced names (`work_lost_events`,
`human_intervention_events`) are documented as amendments below
with their pre-reg footing.

---

## Axis assignment

| metric | axis | source kind |
|---|---|---|
| `work_lost_events` | correctness | run-level event count |
| `human_intervention_events` | correctness | run-level event count (placeholder) |
| `tool_calls_total` | efficiency | run-level count |
| `turns_to_done` | efficiency | run-level count (`Infinite` sentinel) |
| `wall_duration_ms` | efficiency | run-level duration |
| `cost_usd` | efficiency | run-level currency (nullable) |
| `work_redone_turns` | efficiency-adjacent / correctness-adjacent (see note) | per-turn count |

The "adjacent" label on `work_redone_turns` is intentional: it
captures wasted effort (efficiency) caused by a correctness
mishandling (the agent had to redo work). The pre-reg §1.1 places
the underlying signal (`wasted_turns`) on the efficiency axis; the
renderer follows that placement so the dominance table has stable
axis assignment. The "adjacent" framing is for analyst awareness,
not a runtime axis change.

---

## `tool_calls_total`

- **Axis:** efficiency (lower-is-better).
- **Unit:** count.
- **Source field:** `BenchRun.total_tool_calls` (echoes
  `sum(len(turn.tool_calls))` across `transcript.turns`).
- **Counting rule:** integer count of every recorded
  [`ToolCall`](../crates/maw-bench/src/run.rs). Includes tool calls
  that errored (the cost was already paid for the agent to issue
  them). Does NOT include "imagined" calls the agent narrated but
  did not actually issue.
- **Edge cases:** if the harness drops a turn (it must not, but a
  network truncation could), the count is silently low. The
  pre-reg §6.4 manifest is the audit trail.
- **Pre-reg footing:** §1.1, efficiency axis, "tool_calls".

## `turns_to_done`

- **Axis:** efficiency (lower-is-better).
- **Unit:** count.
- **Source field:** `BenchRun.total_turns` when
  `BenchRun.verdict == Success`; otherwise `Infinite` sentinel.
- **Counting rule:** number of agent turns the harness recorded
  while the agent was still producing output. The pre-reg's
  `turns_to_done` semantics is "turns the agent took to finish";
  an unfinished agent has no finite finish-turn count, so the
  metric value is `Infinite`. This sentinel is **not** convertible
  to a finite number for downstream math — the renderer prints
  `INF`; a median computation treats it as the maximum element so
  it tilts the median appropriately without claiming a number.
- **Edge cases:** `RunVerdict::SubstrateIncoherent` counts as not
  Success (so `Infinite`), even though the agent stopped. The
  reasoning: the agent finished the literal turn loop but did not
  finish the planned task — the substrate is broken.
- **Pre-reg footing:** §1.1, efficiency axis, "turns_to_done".

## `wall_duration_ms`

- **Axis:** efficiency (lower-is-better).
- **Unit:** milliseconds.
- **Source field:** `BenchRun.duration_ms`
  (= `manifest.end_ts_unix_ms - manifest.start_ts_unix_ms`).
- **Counting rule:** wall-clock duration of the run, harness-measured.
- **Edge cases:** **NOT a headline metric.** Pre-reg §1.1 records it
  for completeness; SP3 §3 measured CV = 28.4% so it is wall-clock
  noise. The renderer displays it but the publication-grade verdict
  rule (§4.3) does NOT use it.
- **Pre-reg footing:** §1.1, recorded-but-not-headline.

## `cost_usd`

- **Axis:** efficiency (lower-is-better).
- **Unit:** USD; the on-disk representation is `MetricValue::UsdCents`
  with 4-decimal precision (cents × 100) so sub-cent values from
  cheap providers remain visible.
- **Source field:** `BenchRun.cost_usd` (the provider envelope's
  `total_cost_usd`). `None` for `MockAgent`, NaN/negative providers,
  or envelopes that lack the field.
- **Counting rule:** integer count of cents-of-cents derived from
  `(cost * 10_000).round() as u64`. The display rounds to 4 decimals.
- **Edge cases:** `Unavailable` when source is `None`. The renderer
  displays `n/a`. **A missing cost is structurally distinct from
  a zero cost** — never treat one as the other.
- **Pre-reg footing:** §1.1, efficiency axis, "cost_usd".

## `work_lost_events`

- **Axis:** correctness (higher-is-worse; 0 is the bar).
- **Unit:** count.
- **Source fields:** (a) `BenchRun.oracle_b == Red { violations }` —
  count = `violations.len()`; (b) `BenchRun.verdict ==
  SubstrateIncoherent` — count = +1.
- **Counting rule:** additive. A run with N Oracle B violations AND
  `SubstrateIncoherent` verdict has count = N + 1. The two signals
  often coincide (the harness sets `SubstrateIncoherent` when
  Oracle B failed), but the per-event count is the right primitive
  for the dominance table: a substrate that breaks one coherence
  invariant is qualitatively different from one that breaks five,
  and the verdict label adds a +1 because "the agent finished
  oblivious to the breakage" is itself a correctness event.
- **`OracleBSummary::NotApplicable`** contributes 0. Non-maw arms
  do not get phantom correctness scores from Oracle B — they get
  measured against their own substrate's coherence rules per the
  pre-reg §6.4 manifest. (`recoverable_orphaned_work` and
  `irrecoverable_lost_work` from the scenario oracle are NOT in
  the BenchRun schema today; T2.6 / `bn-3l1f` extends the schema
  for them, at which point this metric definition will fold them
  in additively — schema v2.)
- **Pre-reg footing:** **amendment**. The pre-reg §1.1 lists
  `recoverable_orphaned_work`, `irrecoverable_lost_work`,
  `interventions`, `wedge_incident` as the correctness axis. The
  current `BenchRun` schema (T2.2, v1) only carries Oracle B + a
  harness verdict; the scenario-oracle fields wait for T2.6's
  schema extension. `work_lost_events` is the T2.4 conservative
  superset: every signal the v1 schema actually carries that
  indicates "something is broken at run end" rolls up here. When
  T2.6 ships the scenario-oracle fields, this metric is **renamed
  or split** rather than redefined silently — the pre-reg's named
  metrics get their own rows then. This is a deliberate "name the
  amendment, do not hide it" choice per pre-reg §7's discipline.
- **Substrate-op vocabulary mapping:** an `OracleBSummary::Red`
  violation is the substrate-agnostic post-condition of any
  `StepOutcome` sequence that leaves the maw refs in a Prime-
  Invariant-breaching shape (the bn-cm63 class). The
  `StateSnapshot` at run end is the substrate-neutral surface;
  Oracle B reads the per-substrate ref layout to derive the
  violation. For non-maw arms, the analogous derivation lives in
  T2.6's scenario oracle (forthcoming).

## `work_redone_turns`

- **Axis:** efficiency (lower-is-better), with a correctness-adjacent
  reading (the agent had to redo because something broke).
- **Unit:** count of turns.
- **Source field:** `BenchRun.transcript.turns[*].tool_calls[*]`,
  parsed by the heuristic in
  `maw_bench_metrics::extract::count_work_redone_turns`.
- **Counting rule (T2.4 heuristic — the conservative, deterministic
  approximation):** a turn counts as "redone" iff either:
  1. **Recovery entry**: the turn's tool-call set contains a string
     in the conflict-recovery vocabulary
     (`"conflict"`, `"ws conflicts"`, `"resolve"`, `"recover"`,
     `"rebase"`) AND the **prior** turn did not. (A fresh entry
     into recovery, not a continuation.)
  2. **Literal retry**: the turn's tool-call set contains a `Bash`
     call whose `args_json` matches a `Bash` call in the prior
     turn byte-for-byte.
- **Substrate-op vocabulary mapping:** the principled grounding for
  this metric is: count turns where the agent retried after a
  [`StepOutcome { conflicted: true }`](../crates/maw-bench-adapters/src/lib.rs)
  outcome on the substrate. The per-op verbs are not directly
  invoked in a live (agent-driven) SG2 run, but the heuristic
  above is the transcript-side projection — it counts the same
  event class via the only observable channel (tool calls).
- **Edge cases:** the heuristic is intentionally conservative — it
  can under-count (an agent that redoes work using vocabulary not
  on the list) and over-count (a Bash call whose args coincidentally
  contain "rebase" but is not a recovery operation, e.g.
  `git log --merges --grep rebase`). The pre-reg §6.3 specifies
  **blind double-coding by two analysts on a 20% transcript
  sample** for the publication-grade number; that is a human-coded
  pass T2.5 (`bn-1rgk`, maw-per-verb attribution) will deliver
  alongside per-verb event-stream attribution.
- **Pre-reg footing:** **amendment in spirit, alignment in metric
  name.** The pre-reg's efficiency-axis metric is `wasted_turns`
  /`work_redone`; we name ours `work_redone_turns` because the
  bone phrases it that way ("work-redone / wasted-turns"). The
  underlying definition is the same; T2.5 produces the human-coded
  pass that supersedes the heuristic.

## `human_intervention_events`

- **Axis:** correctness (higher-is-worse).
- **Unit:** count.
- **Source field:** **reserved** — `MetricValue::Unavailable` in the
  v1 schema. The BenchRun schema does not currently carry a
  transcript marker for "agent escalated to a human" or "harness
  recorded an out-of-band human action".
- **Counting rule:** today, always `Unavailable`. The metric name
  is stable; the source matures in a later bone.
- **Future signal source (the hook):** any of —
  1. A transcript turn whose `reply_text` matches a pinned
     escalation marker (e.g. "ESCALATE", "HALT", "I need a human").
  2. A harness-side event added to `BenchRun` (schema v2+) that
     records when the agent stopped and a human action followed
     before continuation (e.g. `human_resume_count: u32`).
  3. A substrate-side prompt or refusal that the harness routes
     to a human channel (e.g. `maw ws merge` requesting force
     confirmation in a way the non-interactive agent cannot satisfy).
- **Pre-reg footing:** **amendment**. The pre-reg §1.1 lists
  `interventions` on the correctness axis. We track it under the
  longer name `human_intervention_events` to disambiguate from
  *agent self-interventions* (which the agent counts internally
  and the harness conflates with normal turns). When the source
  matures, the renamed metric replaces this placeholder; tests
  will reject `Unavailable` once `BenchRun` carries the field.

---

## Schema version

- `MetricRecord::SCHEMA_VERSION = 1`. Additive optional fields do
  NOT bump. Field removal or type change bumps.
- `BenchRun::SCHEMA_VERSION = 1` is the upstream schema this version
  consumes. When T2.6 extends BenchRun to v2 (scenario-oracle
  fields), the `work_lost_events` metric definition splits per its
  edge-case section above; `MetricRecord` bumps to v2 in the same
  PR; the doc gets the new sections.

## Renderer invariants (testable)

- The dominance table emits **correctness rows first**, separated by
  a captioned divider from efficiency rows. Caption text is
  load-bearing — readers see it on every render.
- The table has **no row** named "overall", "total", "score",
  "winner", "rank", "ranking", "composite", or "weighted".
- The table has **no column** that combines axes.
- Per-arm aggregation (`--median`) is **within-arm only**; no
  across-arm aggregation produces a single number.
- The header line always carries `axes printed SEPARATELY; no
  cross-axis aggregation` so a screenshot of the table cannot strip
  the rule.

These invariants are asserted by
`crates/maw-bench-metrics/tests/no_composite.rs`.

---

## Downstream constraints (for T2.5 / bn-1rgk)

T2.5 (maw-per-verb attribution) will:

1. Extend `BenchRun` (schema bump) with per-tool-call substrate-op
   attribution. Each `ToolCall` gains an optional
   `attributed_op: Option<OpClass>` plus an optional
   `attributed_outcome: Option<StepOutcome>` so the per-verb mapping
   becomes machine-derivable rather than heuristic-derived.
2. Replace the `count_work_redone_turns` heuristic with the
   attribution-driven count (a turn is redone iff it follows a
   `StepOutcome { conflicted: true }` and re-issues an op of the
   same class on the same target). The metric name does not
   change; the doc section gains a `T2.5 update` subsection.
3. Add a per-verb summary block to the rendered table (still
   per-arm, still no cross-axis aggregation).
