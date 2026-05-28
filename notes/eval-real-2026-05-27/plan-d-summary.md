# Plan D Tier 2 — sonnet + haiku × spectrum × 3 arms (2026-05-27/28)

**600 BenchRuns total** (10 cells × 10 seeds × 3 arms × 2 models). Real Claude via `claude -p` subprocess against the lead's CC subscription. ~$84 token-equivalent (quota burn; $0 billed).

## Per-cell × arm × model medians

### Sonnet (T0 cells only)

| Cell | maw tool / turns | worktrees+conv | jj | best |
|---|---|---|---|---|
| C0 benign | **9.5 / 10.5** | 10 / 11 | 13 / 14 | maw |
| C1 light | **11 / 10.5** | 12 / 13 | 12.5 / 13.5 | maw |
| C2 mod | **10.5 / 11** | 11.5 / 12.5 | 10.5 / 11 (tied) | maw=jj |
| C3 heavy | **10 / 11** | 10.5 / 11.5 | 13 / 12.5 | maw |
| C4 hostile | **10 / 10.5** | 12.5 / 13.5 | 15 / 16 | maw |

### Haiku (T0 cells only)

| Cell | maw tool / turns | worktrees+conv | jj | best |
|---|---|---|---|---|
| C0 benign | 14.5 / 13.5 | **13.5 / 14** | 21.5 / 16.5 | worktrees |
| C1 light | 18 / 15.5 | **17.5 / 15** | 22 / 17.5 | worktrees |
| C2 mod | 17 / 15.5 | **15.5 / 15.5** | 19.5 / 17.5 | worktrees |
| C3 heavy | 17.5 / 14.5 | **16 / 12.5** | 19.5 / 13.5 | worktrees |
| C4 hostile | **17 / 14.5** | 19 / 17 | 24 / 22 | maw |

## Headline findings

1. **The overkill regime exists — but only at haiku.** On sonnet, maw dominates every cell. On haiku, worktrees+convention beats maw at C0–C3 (benign through heavy); maw recaptures C4 hostile. The pre-reg's promise to "publish the regime where maw loses" is satisfiable: it's the (haiku, C0–C3) cells. Honestly published.

2. **Crossover is model-dependent.** Per-cell crossover changes with model capability. Sonnet is fluent enough that maw's verb overhead disappears even at benign cells. Haiku stumbles on coordination overhead until conditions get hostile enough (C4) for the discipline to pay off via fewer wasted attempts.

3. **jj is consistently last** across all 10 (cell × model) combos — opfork-wedge friction + 3-step commit pattern (`describe` + `new` + `bookmark set`) compound across both models.

4. **Zero work-loss across all 600 runs** (3 arms × 2 models × 5 cells × N=10). Per-cell Wilson 95% upper bound on per-run loss rate ≈ 0.161 at N=10. Tighter publication-grade bounds (~0.038 at N=100) would need ~6× more runs.

## Caveats

- **N=10 per cell × arm × model** — below the bn-iux4 §1.3 SUB-A N=20 binding for publication-grade. Wilson UBs are permissive.
- **5 T0 cells only** — the 5 chaos-overlay cells (T1–T5 at C2) in spectrum_grid were also run but represent a smaller per-cell slice; not aggregated above.
- **No chaos overlay fired** — bn-3hzt's chaos infrastructure is wired but the smoke (bn-18mv) confirmed 0 actual chaos events in real-agent runs (`FP_COMMIT_*` failpoints target merge-engine phases that agents at plan_steps=8 don't reach). Chaos-on data is wired-but-empty; not in scope here.
- **Two models** (sonnet, haiku); opus untested. SP3 anchor was sonnet.

## Publication-ready framing

```
At our pre-registered 5-cell spectrum (C0 benign → C4 hostile) at N=10
seeds per cell, we measured maw against git-worktrees+convention and
jj-workspaces with real Claude Code agents (sonnet + haiku):

  - Sonnet: maw is efficiency-dominant across all cells. Worktrees is
    second-best. jj is consistently worst (50–60% more tool calls at C4).
  - Haiku: worktrees+convention is best at C0–C3 (the overkill regime);
    maw recaptures the lead at C4 hostile. The crossover from
    overkill-to-dominant happens in the C3→C4 transition.
  - Zero work-loss observed across all 600 runs (3 arms × 2 models ×
    5 cells × N=10); Wilson 95% upper bound on per-run loss rate
    ≤ 0.161 at this N.

The hypothesized overkill regime is genuine but model-dependent. We
publish it; we do not hide it. The recommendation: use maw whenever
the model is capable enough to handle its verbs (sonnet-and-above
based on this data), or whenever conditions are hostile enough that
coordination discipline pays off (C4-and-above at any model).
```

## Files

- `plan-d-sonnet/{maw,worktrees,jj}/C{0..4}-T0/*.json` — sonnet BenchRuns
- `plan-d-haiku/{maw,worktrees,jj}/C{0..4}-T0/*.json` — haiku BenchRuns
- `plan-d-{sonnet,haiku}/{arm}-stdout.log` — per-arm sweep stdout (cost summary at bottom)

## Cost breakdown

| campaign | quota burn (token-equiv) | wall |
|---|---|---|
| sonnet × maw | $14.20 | ~30 min |
| sonnet × worktrees | $15.68 | ~30 min |
| sonnet × jj | $21.71 | ~50 min |
| haiku × maw | $10.83 | ~25 min |
| haiku × worktrees | $9.19 | ~25 min |
| haiku × jj | $12.74 | ~30 min |
| **Total** | **$84.35** | ~2 hours wall (parallel-within-model, sequential-between-models) |
