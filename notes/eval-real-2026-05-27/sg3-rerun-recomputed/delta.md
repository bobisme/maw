# SG3 Recomputed Decision — bn-27ai Fix A.1/A.2/A.3

Source artifacts: `notes/eval-real-2026-05-27/sg3-rerun`

Recomputed offline against the committed 2026-05-27 BenchRun set using the **fixed** metric pipeline:

- **Fix A.1**: `is_maw_arm` now recognises `maw@<flavor>` arms (was: only `maw` and `maw-*`). The SG3 arms `maw@old-layout` / `maw@new-layout` now route through the principled T2.5 attribution path instead of the substring fallback.
- **Fix A.2** (Approach α): `WsRecoverInvoked` cluster count is decremented by the number of recover-tasks in the scenario prompt's task battery — correctly-executed task-required recovers are no longer mis-classified as friction.
- **Fix A.3**: `sum_proxy` reads the raw per-replicate sum from `CellAggregate::sum` rather than `median × n` (which integer-truncated 1-bit median deltas into N×-amplified totals).

## Verdict

**NO-GO**

Regression: **R6** / **interventions**

By amount: total(new) = 37, total(old) = 34; §3.1 R6 = no net increase (raw per-replicate sum of work_redone_turns; bn-27ai Fix A.3 replaced the lossy median×n proxy)

## R6 raw per-replicate sums (Fix A.3 surface)

| cell | N | raw sum(old) | raw sum(new) | delta |
|------|---|-------------:|-------------:|------:|
| C0-T0 | N_old=20 N_new=20 | 34 | 37 | +3 |
| C2-T0 | N_old=10 N_new=10 | 8 | 12 | +4 |

Pre-fix the same data emitted R6 C2-T0 as `total(new) = 10, total(old) = 0` via `median × n`. Post-fix the totals are the raw per-replicate sums.

## Per-rule evidence

| rule | cell | metric | old | new | status |
|------|------|--------|----:|----:|--------|
| R1 | C0-T0 | irrecoverable_lost_work | 0 | 0 | Pass |
| R2 | C0-T0 | workflow_loss | 0.000 | 0.000 | PassEquivalent |
| R3 | C0-T0 | wedge_incident | 0.000 | 0.000 | PassEquivalent |
| R4 | C0-T0 | turns_to_done | 11 | 12 | Pass |
| R5 | C0-T0 | tool_calls_total | 10 | 11 | Pass |
| R6 | C0-T0 | interventions | 34 | 37 | Fail |
| R1 | C2-T0 | irrecoverable_lost_work | 0 | 0 | Pass |
| R2 | C2-T0 | workflow_loss | 0.000 | 0.000 | PassEquivalent |
| R3 | C2-T0 | wedge_incident | 0.000 | 0.000 | PassEquivalent |
| R4 | C2-T0 | turns_to_done | 10 | 10 | PassEquivalent |
| R5 | C2-T0 | tool_calls_total | 10 | 12 | Pass |
| R6 | C2-T0 | interventions | 8 | 12 | Fail |
