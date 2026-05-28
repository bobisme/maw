# SG3 Final — 2026-05-28 (post-bn-27ai pipeline, post-T3.2 binary)

**Verdict: NO-GO at R6 interventions on both cells.**
**60 BenchRuns, 42 min wall, $9.73 quota.**

## Per-rule table

| Rule | Cell | metric | old (ws/) | new (.maw/) | Status |
|---|---|---|---|---|---|
| R1 | both | irrecoverable_lost_work | 0 | 0 | pass |
| R2 | both | workflow_loss | 0.000 | 0.000 | pass_equivalent |
| R3 | both | wedge_incident | 0.000 | 0.000 | pass_equivalent |
| R4 | both | turns_to_done | 11 / 10 | 11 / 10 | pass_equivalent |
| R5 | C0 / C2 | tool_calls_total | 10 / 10 | 10 / 11 | pass |
| **R6** | **C0** | **interventions** | **31** | **36 (+5)** | **fail** |
| **R6** | **C2** | **interventions** | **8** | **12 (+4)** | **fail** |

## Findings

1. **First SG3 run with correct pipeline and correct binary.** Predecessors (2026-05-26 NO-GO 10v0, 2026-05-27 rerun 10v0) had the metric pipeline bugs that bn-27ai fixed; this is the first measurement that's actually meaningful.

2. **Numbers nearly identical to bn-27ai's offline-recompute prediction** (C0: 34v37 → measured 31v36; C2: 8v12 → measured 8v12). Confirms the prediction; metric pipeline is sound.

3. **Not layout-shape friction.** R4 (turns) and R5 (tool_calls) are equivalent at both cells. Agents do the same total work on both layouts.

4. **Not work loss / not wedge.** R1, R2, R3 all green on both layouts.

5. **Mechanism (per bn-1pzb investigation)**: vocabulary-scarcity friction on `maw recover` — agents fumble `--into` vs `--to` slightly more on the new layout. Possibly because `.maw/workspaces/<name>` paths are longer/different shape and the recover muscle memory misfires.

## Per-iux4 strict interpretation

Per bn-iux4 §3.1 R6 "no net increase" rule applied strictly:
**NO-GO. v1.0 ships on `ws/` layout per the bone language.**

## Lead reframe (2026-05-28 post-eval)

The R6 vocabulary friction is fixable (aliasing `--into` as `--to`, better help output, AGENTS.md hints) — not the real layout concern. The **actual layout concern** is whether agents accidentally edit files in the root/default workspace under the new layout (where source files now live at root vs. nowhere-at-root for ws/).

**SG3 R1-R6 doesn't measure that.** Cross-workspace contamination forensic opened as **bn-das6** — reads same 60 BenchRun transcripts, classifies edits by target path (in-ws / at-root / cross-ws / admin), aggregates per layout, gives a contamination-based recommendation. No new eval $ burn.

## Files

- `maw-{old,new}-layout/C{0,2}-T0/*.json` — 60 BenchRuns (transcripts + tool calls)
- `decision.json` — formal verdict
- `stdout.log` — sweep stdout
