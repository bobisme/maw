# Metric contract (SP3 spike → SG2)

Every agent invocation emits one JSON `result` envelope (`claude -p
--output-format json`). Fields consumed by the benchmark:

| field | type | benchmark meaning |
|---|---|---|
| `total_cost_usd` | float | per-agent cost (secondary metric) |
| `num_turns` | int | agent turns-to-done — **primary ergonomic metric** |
| `is_error` | bool | run health gate; `true` ⇒ discard + rerun |
| `subtype` | str | `success` / `error_max_turns` / `error_max_budget` (wedge proxy) |
| `result` | str | final agent message; scan for `"Not logged in"` (auth fail), abandoned-work language |
| `permission_denials[]` | list | tool friction; non-empty ⇒ scenario mis-scoped |
| `duration_ms` | int | wall clock — NOT a headline metric (CV ~28%) |

Derived per-run benchmark signals (SG2 to compute):

- **wedge_incident**: `subtype != success` OR result mentions divergence/
  abandoned change-ids OR `num_turns > 1.5 × arm-median`.
- **work_redone**: count of abandoned/recreated commits (arm-specific probe
  after the run: jj divergent change-ids / git reflog rewinds / maw
  `ws recover` snapshots touched).
- **interventions**: count of recovery-only operations the agent had to run
  (`jj op integrate`, `jj workspace update-stale`, manual conflict
  resolution) that the maw arm should not require.

Headline = **dominance on `wedge_incident` rate + `work_redone`**, reported
as a crossover curve (regimes where maw loses/is overkill published too —
per the v1.0 strategic posture), NOT a single composite score.
