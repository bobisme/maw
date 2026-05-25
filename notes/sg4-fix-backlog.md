# SG4 fix backlog (T4.1 / bn-120t)

This is the **audit-trail artifact** for T4.1's triage of T2.8's
prioritized friction list (`notes/sg2-friction-list.md`) into concrete
fix child-tasks under SG4 (`bn-2j45`). The bones are the live tracker;
this doc is the row-by-row receipt.

> **TEMPLATE caveat carried over from T2.8.**  The costs below are PILOT
> (synthetic) numbers from `notes/sg2-friction-list.md`. When SG4's
> real-LLM campaign produces friction-bearing transcripts, the
> FrictionList is regenerated (`just sg2-friction-list <artifact-dir>`)
> and T4.1's children are re-ranked per the refresh flow documented in
> `notes/sg2-friction-list-handoff.md`. The cluster-to-fix-class mapping
> is structural and survives re-ranking; only the ordering and the
> per-cluster target-delta baselines move.

## Hard rules in effect

- **NO composite score.** Each fix-task carries its OWN target metric
  delta in its OWN axis (same unit: attributed `total_cost_turns` for
  one named cluster). No cross-cluster aggregation, no "severity
  score". Lifted from T2.4 / T2.8 (`notes/sg2-friction-list-handoff.md`
  "Hard rules").
- **Order is by `total_cost_turns` DESC**, ties broken by stable
  `MawVerbAttribution::ALL` order (mirrors T2.8 friction-list
  tiebreak; asserted in
  `crates/maw-bench-metrics/src/friction_list.rs`).
- **First-pass classifier source.** Pre-reg §6.3 forbids citing these
  numbers in publication text without blind double-coding; SG4 may use
  them for hardening-target selection (which is what T4.1 does here).

## Inclusion rule for `vocabulary_scarcity`

`vocabulary_scarcity` (`MawVerbAttribution::VocabularyScarcity`) is
**always included in the SG4 fix backlog regardless of measured cost**,
including the cost=0 case. Rationale:

- Per `memory:maw-design-rationale-agent-fluency`, the friction the
  jj-substrate-rejection plan was supposed to eliminate has migrated to
  maw's own verbs: "friction moved from 'teach agents jj' to 'teach
  agents maw' (maw's own verbs are training-data-scarce too). maw's
  self-describing, copy-pasteable output is the load-bearing mitigation
  for this, not UX polish. Whether that bet holds in the field is the
  live question."
- The T2.8 friction-list scaffold's "Agent-fluency principle
  measurement" section names this cluster as the open thread.
- Treating "cost=0" as "drop the entry" would erase the *positive*
  evidence we need: a real-run with `vocabulary_scarcity` at 0 across
  two consecutive benches is the *answer* to the open question, not
  noise. The bone must exist to track that answer.

In the current pilot (synthetic) data the cluster's cost is 3, so the
rule does not change the present ordering — but it is documented here so
a future re-rank with cost=0 cannot accidentally drop the entry.

## Wiring

- All fix-task children parented under SG4 (`bn-2j45`).
- Each fix-task is `blocks` for T4.2 (`bn-350o`) — T4.2 is the
  umbrella "harden complete" rollup per its own description and cannot
  declare completion until each child fix lands. T4.3 (`bn-1qty`,
  re-benchmark) already chains from T4.2 (`bn-350o` blocks `bn-1qty`),
  so the validation loop (re-run → confirm reduction) is the dependent
  of the entire backlog.

## Backlog rows (ordered by pilot `total_cost_turns` DESC)

| Rank | Bone | Cluster (`MawVerbAttribution`) | Pilot cost (turns) | Runs | Size | Candidate mitigation class | Target metric delta |
|---:|---|---|---:|---:|:---:|---|---|
| 1 | `bn-yyx`  | `ws_merge_structured_conflict` | 9 | 3 | l | merge-engine-resilience (first-class conflict objects, event log, mergeback queue) | reduce cluster `total_cost_turns` ≥ 50% at next-bench |
| 2 | `bn-221b` | `ws_sync_stale_workspace`      | 3 | 2 | m | stale-state-self-healing (`maw status --json`, safe-cleanup vocabulary, event log) | reduce cluster `total_cost_turns` ≥ 50% at next-bench |
| 3 | `bn-1ieb` | `epoch_sync_required`          | 3 | 2 | m | epoch-auto-advance (architectural, `maw doctor`/`repair` coverage, event log) | reduce cluster `total_cost_turns` ≥ 50% at next-bench |
| 4 | `bn-1t17` | `vocabulary_scarcity`          | 3 | 2 | m | verb-discoverability (`maw crib <agent>`, overkill-line CLI guidance, self-describing output) | reduce cluster `total_cost_turns` ≥ 50% at next-bench; if cost=0 at baseline, "remains 0 across the next two benches" |
| 5 | `bn-29fi` | `ws_recover_invoked`           | 2 | 2 | m | destroy-prevention (`maw doctor`/`repair` coverage, safe-cleanup vocabulary, mergeback queue) | reduce cluster `total_cost_turns` ≥ 50% at next-bench |
| 6 | `bn-c6l3` | `ws_destroy_refused`           | 1 | 1 | s | destroy-guidance-output (self-describing refusal output, safe-cleanup vocabulary, `maw status --json`) | reduce cluster `total_cost_turns` ≥ 50% at next-bench; practical: "reaches 0" |
| 7 | `bn-242l` | `read_from_stale_workspace`    | 1 | 1 | s | status-output-discoverability (machine-readable workspace manifest, `maw status --json`, safe-cleanup vocabulary) | reduce cluster `total_cost_turns` ≥ 50% at next-bench; practical: "reaches 0" |

**Owner column:** the fix-task bones themselves; T4.2 (`bn-350o`) is the
umbrella that rolls them up.

## Unattributed bucket (carried forward)

`total_unattributed_wasted_turns` = **5** (pilot synthetic). NOT a
fix-task target on its own per the T2.8 handoff hard rule
("Surface the unattributed bucket alongside the targets so the residual
blind spot stays visible"); flagged for human coding follow-up per
pre-reg §6.3. When real-run data lands, growth >20% blocks the SG4
stop-condition independent of cluster reductions.

## Refresh flow (re-rank on real-run data)

When T2's real-LLM campaign finishes and `just sg2-friction-list
<artifact-dir>` produces the real-run FrictionList JSON:

1. Re-read `notes/sg2-friction-list.md` for the new ranking.
2. Re-sort the rows in this doc by the new `total_cost_turns` DESC
   (ties: `MawVerbAttribution::ALL`); update each fix-task bone's
   "current measured cost" section via `bn bone comment add` (do NOT
   recreate the bones — keep terseids stable).
3. If a NEW cluster appears in the top-7 that was absent in pilot,
   create an additional fix-task under SG4 with the same shape.
4. If `vocabulary_scarcity` drops to 0, keep its bone (per the
   inclusion rule above); the target delta becomes "stays 0 across the
   next two benches".

## References

- `notes/sg2-friction-list.md` (T2.8 scaffold; source ranking).
- `notes/sg2-friction-list-handoff.md` (T2.8 consumer contract;
  validation loop, stop conditions, no-composite rule).
- `crates/maw-bench-metrics/src/attribution.rs`
  (`MawVerbAttribution` 12-variant vocabulary;
  `MawVerbAttribution::ALL` tiebreak order).
- `crates/maw-bench-metrics/src/friction_list.rs`
  (`FrictionList` schema; `recommended_fix_class` hints).
- `bn-2j45` (SG4 parent; review-pass-1 9-suggestion mitigation menu in
  the single existing comment).
- `memory:maw-design-rationale-agent-fluency`
  (vocabulary-scarcity inclusion rule's design rationale).
