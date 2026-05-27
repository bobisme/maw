# SG2 cross-arm N=10 — 2026-05-27 (sonnet, real Claude)

**Spend:** $10.18 token-equivalent (subscription quota, $0 billed)
**Wall:** ~50 min sequential (maw → worktrees → jj)
**Bundles:** 60 (10 seeds × 2 cells × 3 arms)
**Binary:** maw v0.61.0 (pre-T3.2; layout-flavor not relevant for SG2 maw arm which uses ws/)

## Per-cell × arm medians

| Cell | Metric | maw | worktrees+convention | jj |
|---|---|---|---|---|
| C0 benign | tool_calls_total | **9.5** | 12.5 | 14 |
| C0 benign | turns_to_done | **10.5** | 13.5 | 15 |
| C0 benign | cost_usd (sonnet) | **$0.141** | $0.159 | $0.212 |
| C0 benign | work_lost (out of N=10) | 0 | 0 | 0 |
| C4 hostile | tool_calls_total | **10** | 12.5 | 16 |
| C4 hostile | turns_to_done | **10.5** | 13.5 | 16.5 |
| C4 hostile | cost_usd (sonnet) | **$0.134** | $0.172 | $0.225 |
| C4 hostile | work_lost (out of N=10) | 0 | 0 | 0 |

## Findings

1. **maw dominates on every efficiency metric** at both cells. Lower turns, lower tool calls, lower per-run cost.
2. **jj is consistently worst**, with 47–68% more tool calls / cost than maw. Notably the gap exists even at benign C0 where the opfork-wedge shouldn't be biting; suggests structural overhead (likely the `describe / new / bookmark set` 3-step commit pattern vs git's single-step) on top of the C4 wedge cost.
3. **Worktrees+convention sits in the middle** — its thin coordination convention adds steps over maw's built-ins but avoids jj's working-copy-as-a-commit overhead.
4. **Zero work-loss across all 30 runs × 3 arms = 90 runs** at N=10. Wilson 95% UB per cell × arm ≈ 0.278; not publication-tight (need N≥20 for ≤0.161 UB per bn-iux4 §1.3) but rules out high-rate loss.
5. **The overkill regime the pre-reg said we'd publish did NOT emerge.** At both pre-registered cells (C0 benign + C4 hostile) at sonnet, maw is efficiency-dominant. This is a stronger claim than promised; v1.0 publication may report "across our pre-registered grid at sonnet, maw was efficiency-dominant; the predicted overkill regime did not emerge at N=10."

## Caveats

- N=10 per cell × arm is below the bn-iux4 publication-grade N=20 SUB-A binding.
- Two cells (C0 + C4) only; the full frozen grid (per T2.7) is larger.
- One model (sonnet); cross-model behavior tracked separately as bn-3w0c.
- The "work_redone_turns" metric is the T2.4 heuristic (T2.5 attribution-driven replacement not yet measuring — this matters for SG4 re-bench, not SG2 baseline).
